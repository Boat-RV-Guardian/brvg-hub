"""Control: the close button and the open action.

What these pin is the ladder, from the client's side. The cloud enforces it too — every command is
re-checked server-side against the STORED scope — but a client that renders a control it cannot use
teaches the user that the integration is broken, so the gating has to be right in both places.
"""

from __future__ import annotations

from homeassistant.const import ATTR_ENTITY_ID
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError
from homeassistant.helpers.aiohttp_client import async_get_clientsession
import pytest

from custom_components.boatrvguardian.api import (
    BrvgClient,
    BrvgError,
    BrvgPlanError,
    BrvgScopeError,
)
from custom_components.boatrvguardian.const import DOMAIN

from .conftest import HOST, TOKEN, VID, setup_integration, vehicle_payload

CONTROL_URL = f"{HOST}/api/v1/control"
BUTTON = "button.serenity_close_water_valve"


def valve_payload(scope: str, present: bool = True):
    return vehicle_payload(scope=scope, valve={"present": present})


# ── the button appears only when it would WORK ────────────────────────────────────────────────────


@pytest.mark.parametrize("scope", ["safe", "control"])
async def test_close_button_exists_for_scopes_that_may_close(hass, aioclient_mock, scope) -> None:
    await setup_integration(hass, aioclient_mock, valve_payload(scope))
    assert hass.states.get(BUTTON) is not None


async def test_no_close_button_for_a_read_only_token(hass, aioclient_mock) -> None:
    """A control guaranteed to 403 is worse than no control — it reads as a broken integration."""
    await setup_integration(hass, aioclient_mock, valve_payload("read"))
    assert hass.states.get(BUTTON) is None


async def test_no_close_button_when_the_vehicle_has_no_valve(hass, aioclient_mock) -> None:
    """Otherwise "no valve aboard" is indistinguishable from "not allowed"."""
    await setup_integration(hass, aioclient_mock, valve_payload("control", present=False))
    assert hass.states.get(BUTTON) is None


async def test_pressing_close_sends_the_close_command(hass, aioclient_mock) -> None:
    await setup_integration(hass, aioclient_mock, valve_payload("safe"))
    aioclient_mock.post(CONTROL_URL, json={"status": "ok", "action": "close", "valves": 1})

    await hass.services.async_call("button", "press", {ATTR_ENTITY_ID: BUTTON}, blocking=True)

    posts = [c for c in aioclient_mock.mock_calls if str(c[1]).endswith("/api/v1/control")]
    assert posts[-1][2] == {"vid": VID, "action": "close"}
    assert posts[-1][3]["Authorization"] == f"Bearer {TOKEN}"


async def test_a_failed_close_is_raised_not_swallowed(hass, aioclient_mock) -> None:
    """A close that silently failed is the worst outcome here: the user walks away believing the
    water is off."""
    await setup_integration(hass, aioclient_mock, valve_payload("safe"))
    aioclient_mock.post(CONTROL_URL, status=502, json={"error": "relay down"})

    with pytest.raises(HomeAssistantError):
        await hass.services.async_call("button", "press", {ATTR_ENTITY_ID: BUTTON}, blocking=True)


# ── opening ───────────────────────────────────────────────────────────────────────────────────────


async def test_open_sends_the_duration_in_seconds(hass, aioclient_mock) -> None:
    await setup_integration(hass, aioclient_mock, valve_payload("control"))
    aioclient_mock.post(CONTROL_URL, json={"status": "ok", "action": "open", "valves": 1})

    await hass.services.async_call(DOMAIN, "open_valve", {"duration_minutes": 20}, blocking=True)

    # NOT mock_calls[-1]: opening triggers a refresh, so the last call is the follow-up GET. That
    # refresh is deliberate — the state a user sees should reflect the command they just sent.
    posts = [c for c in aioclient_mock.mock_calls if str(c[1]).endswith("/api/v1/control")]
    assert posts[-1][2] == {"vid": VID, "action": "open", "durationSec": 1200}


async def test_open_is_refused_for_a_safe_token_before_any_request(hass, aioclient_mock) -> None:
    """Named locally rather than left to the 403, so the user is told the actual fix: mint a token
    that can open."""
    await setup_integration(hass, aioclient_mock, valve_payload("safe"))
    before = len(aioclient_mock.mock_calls)

    with pytest.raises(ServiceValidationError, match="open and close"):
        await hass.services.async_call(
            DOMAIN, "open_valve", {"duration_minutes": 20}, blocking=True
        )
    assert len(aioclient_mock.mock_calls) == before  # nothing was sent


async def test_open_is_refused_when_there_is_no_valve(hass, aioclient_mock) -> None:
    await setup_integration(hass, aioclient_mock, valve_payload("control", present=False))
    with pytest.raises(ServiceValidationError, match="no valve"):
        await hass.services.async_call(
            DOMAIN, "open_valve", {"duration_minutes": 20}, blocking=True
        )


# The valve may never run unbounded. The schema is what guarantees a caller cannot omit the bound
# and inherit a silent maximum.
async def test_open_requires_a_duration(hass, aioclient_mock) -> None:
    await setup_integration(hass, aioclient_mock, valve_payload("control"))
    with pytest.raises(vol_invalid()):
        await hass.services.async_call(DOMAIN, "open_valve", {}, blocking=True)


@pytest.mark.parametrize("bad", [0, -5, 100_000])
async def test_open_rejects_a_duration_outside_the_bounds(hass, aioclient_mock, bad) -> None:
    await setup_integration(hass, aioclient_mock, valve_payload("control"))
    with pytest.raises(vol_invalid()):
        await hass.services.async_call(
            DOMAIN, "open_valve", {"duration_minutes": bad}, blocking=True
        )


def vol_invalid():
    import voluptuous as vol

    return vol.Invalid


# ── the client's own error mapping ────────────────────────────────────────────────────────────────


def client(hass) -> BrvgClient:
    return BrvgClient(async_get_clientsession(hass), HOST, VID, TOKEN)


async def test_a_scope_403_is_not_a_plan_403(hass, aioclient_mock) -> None:
    """Different fixes: one needs a new token, the other needs a different plan. Neither is a retry,
    and neither is reauth — which is why this is not an auth error."""
    aioclient_mock.post(
        CONTROL_URL, status=403, json={"error": "forbidden: this token is read-only"}
    )
    with pytest.raises(BrvgScopeError):
        await client(hass).async_control_valve("close")


async def test_a_plan_403_is_a_plan_error(hass, aioclient_mock) -> None:
    aioclient_mock.post(
        CONTROL_URL, status=403, json={"error": "forbidden: plan does not include remote control"}
    )
    with pytest.raises(BrvgPlanError):
        await client(hass).async_control_valve("open", duration_minutes=5)


async def test_the_client_refuses_an_open_with_no_duration(hass, aioclient_mock) -> None:
    with pytest.raises(BrvgError, match="requires a duration"):
        await client(hass).async_control_valve("open")
