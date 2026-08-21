"""Polling cadence and failure handling.

The interval is not a setting and not a guess: the cloud reports the plan's telemetry resolution and
rate-limits to exactly that, so the server is the authority and the coordinator follows it.
"""

from __future__ import annotations

from datetime import timedelta

from homeassistant.config_entries import ConfigEntryState
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.boatrvguardian.const import (
    DOMAIN,
    MAX_POLL_SECONDS,
    MIN_POLL_SECONDS,
)

from .conftest import ENTRY_DATA, VEHICLE_URL, VID, vehicle_payload


async def setup(hass, aioclient_mock, **payload):
    aioclient_mock.get(VEHICLE_URL, json=vehicle_payload(**payload))
    entry = MockConfigEntry(domain=DOMAIN, unique_id=VID, data=ENTRY_DATA)
    entry.add_to_hass(hass)
    await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    return entry


async def test_follows_the_plans_cadence(hass, aioclient_mock) -> None:
    entry = await setup(hass, aioclient_mock, resolutionSec=300)
    assert entry.runtime_data.update_interval == timedelta(seconds=300)


async def test_never_polls_faster_than_the_floor(hass, aioclient_mock) -> None:
    """A cloud that reported 1s must not turn the integration into a hammer."""
    entry = await setup(hass, aioclient_mock, resolutionSec=1)
    assert entry.runtime_data.update_interval == timedelta(seconds=MIN_POLL_SECONDS)


async def test_never_polls_so_slowly_it_looks_frozen(hass, aioclient_mock) -> None:
    entry = await setup(hass, aioclient_mock, resolutionSec=99_999)
    assert entry.runtime_data.update_interval == timedelta(seconds=MAX_POLL_SECONDS)


async def test_ignores_a_nonsense_resolution_rather_than_trusting_it(hass, aioclient_mock) -> None:
    entry = await setup(hass, aioclient_mock, resolutionSec=0)
    assert entry.runtime_data.update_interval == timedelta(seconds=MAX_POLL_SECONDS)


async def test_a_rejected_token_asks_the_user_rather_than_going_quiet(hass, aioclient_mock) -> None:
    """A revoked or rotated token is the expected end, and it must surface as reauth."""
    aioclient_mock.get(VEHICLE_URL, status=401, json={})
    entry = MockConfigEntry(domain=DOMAIN, unique_id=VID, data=ENTRY_DATA)
    entry.add_to_hass(hass)
    await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()

    assert entry.state is ConfigEntryState.SETUP_ERROR
    assert any(
        f["context"]["source"] == "reauth" for f in hass.config_entries.flow.async_progress()
    )


async def test_a_downgraded_plan_retries_instead_of_asking_for_a_new_token(
    hass, aioclient_mock
) -> None:
    """No token fixes a Dockside vehicle, so prompting for one would be a dead end."""
    aioclient_mock.get(VEHICLE_URL, status=403, json={})
    entry = MockConfigEntry(domain=DOMAIN, unique_id=VID, data=ENTRY_DATA)
    entry.add_to_hass(hass)
    await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()

    assert entry.state is ConfigEntryState.SETUP_RETRY
    assert not hass.config_entries.flow.async_progress()


async def test_asking_too_soon_keeps_the_last_state_and_backs_off(hass, aioclient_mock) -> None:
    """A 429 is not a failure — nothing could have changed inside the window."""
    entry = await setup(hass, aioclient_mock, resolutionSec=30)
    coordinator = entry.runtime_data
    before = coordinator.data

    aioclient_mock.clear_requests()
    aioclient_mock.get(VEHICLE_URL, status=429, headers={"Retry-After": "120"}, json={})
    await coordinator.async_refresh()

    assert coordinator.last_update_success is True  # entities stay available
    assert coordinator.data == before  # and keep showing the truth
    assert coordinator.update_interval == timedelta(seconds=120)


async def test_a_tiny_retry_after_still_respects_the_floor(hass, aioclient_mock) -> None:
    entry = await setup(hass, aioclient_mock, resolutionSec=30)
    coordinator = entry.runtime_data

    aioclient_mock.clear_requests()
    aioclient_mock.get(VEHICLE_URL, status=429, headers={"Retry-After": "1"}, json={})
    await coordinator.async_refresh()

    assert coordinator.update_interval == timedelta(seconds=MIN_POLL_SECONDS)


async def test_the_vehicle_is_a_device_so_its_sensors_have_a_home(hass, aioclient_mock) -> None:
    from homeassistant.helpers import device_registry as dr

    await setup(hass, aioclient_mock)
    device = dr.async_get(hass).async_get_device(identifiers={(DOMAIN, VID)})
    assert device is not None
    assert device.name == "Serenity"
