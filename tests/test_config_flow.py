"""Setup and reauth.

Validation here is a REAL read, not a shape check: the only way to know a token works is to use it,
and finding out at setup is far kinder than a config entry that silently never updates.
"""

from __future__ import annotations

from homeassistant import config_entries
from homeassistant.data_entry_flow import FlowResultType
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.boatrvguardian.const import CONF_HOST, CONF_TOKEN, CONF_VID, DOMAIN

from .conftest import ENTRY_DATA, HOST, TOKEN, VEHICLE_URL, VID, vehicle_payload


async def start(hass):
    return await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": config_entries.SOURCE_USER}
    )


async def test_creates_an_entry_titled_with_the_vessel_name(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, json=vehicle_payload())
    result = await hass.config_entries.flow.async_configure(
        (await start(hass))["flow_id"], ENTRY_DATA
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    # The vessel's name, not the vid — the user named their boat and that is what belongs on the card.
    assert result["title"] == "Serenity"
    assert result["data"] == ENTRY_DATA


async def test_falls_back_to_the_vid_when_the_vessel_is_unnamed(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, json=vehicle_payload(name=""))
    result = await hass.config_entries.flow.async_configure(
        (await start(hass))["flow_id"], ENTRY_DATA
    )
    assert result["title"] == VID


async def test_a_rejected_token_says_so_and_stays_on_the_form(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, status=401, json={})
    result = await hass.config_entries.flow.async_configure(
        (await start(hass))["flow_id"], ENTRY_DATA
    )
    assert result["type"] is FlowResultType.FORM
    assert result["errors"] == {"base": "invalid_auth"}


# Three different failures, three different messages. Collapsing them into "cannot_connect" would
# send a Dockside customer hunting a network problem that does not exist.
async def test_a_dockside_vehicle_is_told_about_the_plan(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, status=403, json={})
    result = await hass.config_entries.flow.async_configure(
        (await start(hass))["flow_id"], ENTRY_DATA
    )
    assert result["errors"] == {"base": "plan_excluded"}


async def test_asking_too_soon_is_not_reported_as_a_network_failure(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, status=429, json={})
    result = await hass.config_entries.flow.async_configure(
        (await start(hass))["flow_id"], ENTRY_DATA
    )
    assert result["errors"] == {"base": "rate_limited"}


async def test_an_unreachable_cloud_is_cannot_connect(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, exc=TimeoutError)
    result = await hass.config_entries.flow.async_configure(
        (await start(hass))["flow_id"], ENTRY_DATA
    )
    assert result["errors"] == {"base": "cannot_connect"}


async def test_the_same_vehicle_cannot_be_added_twice(hass, aioclient_mock) -> None:
    """One token reads one vehicle, so one entry IS one vehicle."""
    MockConfigEntry(domain=DOMAIN, unique_id=VID, data=ENTRY_DATA).add_to_hass(hass)
    aioclient_mock.get(VEHICLE_URL, json=vehicle_payload())
    result = await hass.config_entries.flow.async_configure(
        (await start(hass))["flow_id"], ENTRY_DATA
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "already_configured"


async def test_reauth_replaces_the_token_in_place(hass, aioclient_mock) -> None:
    """Rotating a token in the app is the EXPECTED way an entry stops working."""
    entry = MockConfigEntry(domain=DOMAIN, unique_id=VID, data=ENTRY_DATA)
    entry.add_to_hass(hass)

    result = await entry.start_reauth_flow(hass)
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "reauth_confirm"

    aioclient_mock.get(VEHICLE_URL, json=vehicle_payload())
    done = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_TOKEN: "brvg_rotated"}
    )
    # The successful reauth reloads the entry; let that finish or it outlives the test.
    await hass.async_block_till_done()

    assert done["type"] is FlowResultType.ABORT
    assert done["reason"] == "reauth_successful"
    # The vid and host survive: only the credential changed.
    assert entry.data == {CONF_VID: VID, CONF_TOKEN: "brvg_rotated", CONF_HOST: HOST}


async def test_reauth_rejects_a_token_that_still_does_not_work(hass, aioclient_mock) -> None:
    entry = MockConfigEntry(domain=DOMAIN, unique_id=VID, data=ENTRY_DATA)
    entry.add_to_hass(hass)
    result = await entry.start_reauth_flow(hass)

    aioclient_mock.get(VEHICLE_URL, status=401, json={})
    again = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_TOKEN: "brvg_stillwrong"}
    )
    assert again["type"] is FlowResultType.FORM
    assert again["errors"] == {"base": "invalid_auth"}
    assert entry.data[CONF_TOKEN] == TOKEN  # unchanged
