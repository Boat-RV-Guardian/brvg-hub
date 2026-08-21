"""What the entities say.

The alarm decision is the cloud's — `kind` arrives classified precisely so no Python copy of the
worker's event regexes can drift from it. These tests pin that the component CONSUMES that answer
rather than second-guessing it, and that a device going quiet is never mistaken for good news.
"""

from __future__ import annotations

from datetime import UTC, datetime

from homeassistant.components.binary_sensor import BinarySensorDeviceClass
from homeassistant.config_entries import ConfigEntryState
from homeassistant.const import STATE_OFF, STATE_ON, STATE_UNAVAILABLE
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.boatrvguardian.const import DOMAIN

from .conftest import ENTRY_DATA, VEHICLE_URL, VID, vehicle_payload


def device(device_id: str, **over):
    base = {
        "id": device_id,
        "event": "flood.alarm",
        "kind": "alarm",
        "category": "flood",
        "at": 1_785_900_000_000,
        "extra": {},
    }
    return {**base, **over}


async def setup(hass, aioclient_mock, devices):
    aioclient_mock.get(VEHICLE_URL, json=vehicle_payload(devices=devices))
    entry = MockConfigEntry(domain=DOMAIN, unique_id=VID, data=ENTRY_DATA)
    entry.add_to_hass(hass)
    await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    return entry


async def test_alarm_is_on_only_for_an_alarm(hass, aioclient_mock) -> None:
    await setup(
        hass,
        aioclient_mock,
        [
            device("bilge", kind="alarm"),
            device("dry", kind="clear", event="flood.alarm_off"),
            device("volts", kind="telemetry", event="voltmeter.change", category="battery"),
            device("fan", kind="state", event="switch.on", category=None),
        ],
    )
    assert hass.states.get("binary_sensor.bilge_alarm").state == STATE_ON
    # A cleared alarm, a voltage reading and a relay flipping are all NOT alarms. `switch.off`
    # matching the cleared-alarm spelling by coincidence is the bug this classification prevents.
    for entity in (
        "binary_sensor.dry_alarm",
        "binary_sensor.volts_alarm",
        "binary_sensor.fan_alarm",
    ):
        assert hass.states.get(entity).state == STATE_OFF, entity


async def test_device_class_follows_the_category(hass, aioclient_mock) -> None:
    await setup(
        hass,
        aioclient_mock,
        [
            device("bilge", category="flood"),
            device("shore", category="shore_power"),
            device("cabin", category="climate"),
            device("odd", category="voyage_history_or_something_new"),
            device("none", category=None),
        ],
    )

    def dc(entity_id: str):
        return hass.states.get(entity_id).attributes["device_class"]

    assert dc("binary_sensor.bilge_alarm") == BinarySensorDeviceClass.MOISTURE
    assert dc("binary_sensor.shore_alarm") == BinarySensorDeviceClass.PLUG
    assert dc("binary_sensor.cabin_alarm") == BinarySensorDeviceClass.HEAT
    # An unmapped or brand-new category is PROBLEM, not a guess. A wrong device class renders a wrong
    # icon and a wrong on/off vocabulary on every dashboard.
    assert dc("binary_sensor.odd_alarm") == BinarySensorDeviceClass.PROBLEM
    assert dc("binary_sensor.none_alarm") == BinarySensorDeviceClass.PROBLEM


async def test_last_event_carries_the_reading_and_the_classification(hass, aioclient_mock) -> None:
    await setup(
        hass,
        aioclient_mock,
        [
            device(
                "shore",
                event="pm1.voltage_change",
                kind="telemetry",
                category="shore_power",
                extra={"volts": "119.4", "amps": "3.2"},
            ),
        ],
    )
    state = hass.states.get("sensor.shore_last_event")
    assert state.state == "pm1.voltage_change"
    assert state.attributes["kind"] == "telemetry"
    assert state.attributes["category"] == "shore_power"
    assert state.attributes["telemetry_volts"] == "119.4"
    assert state.attributes["telemetry_amps"] == "3.2"


async def test_last_report_is_a_real_timestamp(hass, aioclient_mock) -> None:
    """The wire carries UTC epoch ms; a formatted local string would be a time-policy violation."""
    await setup(hass, aioclient_mock, [device("bilge", at=1_785_900_000_000)])
    state = hass.states.get("sensor.bilge_last_report")
    assert state.state == datetime.fromtimestamp(1_785_900_000, tz=UTC).isoformat()


async def test_a_device_with_no_usable_time_is_unknown_not_epoch_zero(hass, aioclient_mock) -> None:
    await setup(hass, aioclient_mock, [device("bilge", at=0)])
    assert hass.states.get("sensor.bilge_last_report").state == "unknown"


# ⚠️ The one that matters most on a boat.
async def test_a_device_that_goes_quiet_is_UNAVAILABLE_never_off(hass, aioclient_mock) -> None:
    """A silent bilge sensor must never render as a dry bilge."""
    entry = await setup(hass, aioclient_mock, [device("bilge", kind="alarm"), device("shore")])
    assert hass.states.get("binary_sensor.bilge_alarm").state == STATE_ON

    aioclient_mock.clear_requests()
    aioclient_mock.get(VEHICLE_URL, json=vehicle_payload(devices=[device("shore")]))
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()

    assert hass.states.get("binary_sensor.bilge_alarm").state == STATE_UNAVAILABLE
    assert hass.states.get("sensor.bilge_last_event").state == STATE_UNAVAILABLE
    assert hass.states.get("binary_sensor.shore_alarm").state is not None  # the rest carry on


async def test_entities_are_uniquely_identified_per_vehicle_and_device(
    hass, aioclient_mock
) -> None:
    """Two boats with a device called `bilge` must not collide."""
    from homeassistant.helpers import entity_registry as er

    await setup(hass, aioclient_mock, [device("bilge")])
    registered = er.async_get(hass).async_get("binary_sensor.bilge_alarm")
    assert registered.unique_id == f"{VID}_bilge_alarm"


async def test_unloading_leaves_nothing_claiming_to_know_anything(hass, aioclient_mock) -> None:
    """Home Assistant keeps a RESTORED placeholder for a registered entity rather than deleting the
    state, so the check that matters is that it stops asserting a reading — not that it vanishes."""
    entry = await setup(hass, aioclient_mock, [device("bilge")])
    assert hass.states.get("binary_sensor.bilge_alarm").state == STATE_ON

    assert await hass.config_entries.async_unload(entry.entry_id)
    await hass.async_block_till_done()

    assert entry.state is ConfigEntryState.NOT_LOADED
    assert hass.states.get("binary_sensor.bilge_alarm").state == STATE_UNAVAILABLE
