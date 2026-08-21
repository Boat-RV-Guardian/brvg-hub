"""Alarm state, one binary sensor per device.

The alarm/clear decision is NOT made here — the cloud reports `kind` on every device precisely so no
client re-implements its event classifier. Two bugs in this platform's history came from a classifier
that disagreed with itself; a Python copy of those regexes would be a third. If you are ever tempted
to parse an event NAME in this file, the fix belongs in the API.
"""

from __future__ import annotations

from homeassistant.components.binary_sensor import (
    BinarySensorDeviceClass,
    BinarySensorEntity,
)
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import BrvgConfigEntry
from .const import (
    CATEGORY_CLIMATE,
    CATEGORY_FLOOD,
    CATEGORY_PRESENCE,
    CATEGORY_SECURITY,
    CATEGORY_SHORE_POWER,
    KIND_ALARM,
)
from .entity import BrvgEntity

# Only where Home Assistant has a class that genuinely matches. Everything else is PROBLEM, which is
# honest: a wrong device class renders a wrong icon and a wrong on/off vocabulary in every dashboard.
DEVICE_CLASSES = {
    CATEGORY_FLOOD: BinarySensorDeviceClass.MOISTURE,
    CATEGORY_SHORE_POWER: BinarySensorDeviceClass.PLUG,
    CATEGORY_CLIMATE: BinarySensorDeviceClass.HEAT,
    CATEGORY_SECURITY: BinarySensorDeviceClass.SAFETY,
    CATEGORY_PRESENCE: BinarySensorDeviceClass.MOTION,
}


async def async_setup_entry(
    hass: HomeAssistant, entry: BrvgConfigEntry, async_add_entities: AddEntitiesCallback
) -> None:
    coordinator = entry.runtime_data
    async_add_entities(
        BrvgAlarmBinarySensor(coordinator, device["id"])
        for device in coordinator.devices
        if device.get("id")
    )


class BrvgAlarmBinarySensor(BrvgEntity, BinarySensorEntity):
    """On when this device is currently in alarm."""

    _attr_translation_key = "alarm"

    def __init__(self, coordinator, device_id: str) -> None:
        super().__init__(coordinator, device_id)
        vid = str((coordinator.data or {}).get("vid") or "")
        self._attr_unique_id = f"{vid}_{device_id}_alarm"

    @property
    def device_class(self) -> BinarySensorDeviceClass:
        category = self.device_data.get("category")
        return DEVICE_CLASSES.get(category, BinarySensorDeviceClass.PROBLEM)

    @property
    def is_on(self) -> bool | None:
        kind = self.device_data.get("kind")
        if kind is None:
            return None
        # Only `alarm` is on. `clear` is off; `telemetry` and `state` are not alarm-shaped events at
        # all, so a device whose last word was a voltage reading is not thereby in alarm.
        return kind == KIND_ALARM
