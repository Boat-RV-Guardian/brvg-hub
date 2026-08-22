"""Shared fixtures.

`enable_custom_integrations` is what lets Home Assistant load a component out of
`custom_components/` at all; without it every setup call fails with "integration not found" and the
reason is not obvious from the traceback.
"""

from __future__ import annotations

from typing import Any

import pytest

from custom_components.boatrvguardian.const import CONF_HOST, CONF_TOKEN, CONF_VID

pytest_plugins = "pytest_homeassistant_custom_component"

HOST = "https://api.example.com"
VID = "v1"
TOKEN = "brvg_testtoken"

ENTRY_DATA = {CONF_VID: VID, CONF_TOKEN: TOKEN, CONF_HOST: HOST}

VEHICLE_URL = f"{HOST}/api/v1/vehicle"
HISTORY_URL = f"{HOST}/api/v1/history"


async def setup_integration(hass, aioclient_mock, payload=None):
    """Configure one vehicle and return its entry, with the vehicle read already mocked."""
    from pytest_homeassistant_custom_component.common import MockConfigEntry

    from custom_components.boatrvguardian.const import DOMAIN

    aioclient_mock.get(VEHICLE_URL, json=payload if payload is not None else vehicle_payload())
    entry = MockConfigEntry(domain=DOMAIN, data=ENTRY_DATA, unique_id=VID)
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    return entry


@pytest.fixture(autouse=True)
def auto_enable_custom_integrations(enable_custom_integrations):
    """Load the component from custom_components/ rather than HA's own tree."""
    yield


def vehicle_payload(**over: Any) -> dict[str, Any]:
    """A vehicle as the cloud reports it, with `kind` and `category` already classified."""
    return {
        "vid": VID,
        "name": "Serenity",
        "vehicleType": "boat",
        "tier": "premium",
        "resolutionSec": 30,
        "scope": "read",
        "valve": {"present": False},
        "devices": [
            {
                "id": "bilge",
                "event": "flood.alarm",
                "kind": "alarm",
                "category": "flood",
                "at": 1_785_900_000_000,
                "extra": {"depth": "12"},
            },
            {
                "id": "shore",
                "event": "pm1.voltage_change",
                "kind": "telemetry",
                "category": "shore_power",
                "at": 1_785_900_000_000,
                "extra": {"volts": "119.4"},
            },
        ],
        **over,
    }
