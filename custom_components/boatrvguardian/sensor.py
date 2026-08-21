"""The last event each device reported, plus its telemetry as attributes.

Deliberately ONE sensor per device rather than one per telemetry key. The `extra` map is
vendor-shaped and open-ended — a Shelly voltmeter, an H&T probe and a LinkTap gateway do not agree on
key names or units — so minting an entity per key would create entities that appear and vanish as
hardware changes, with units this component would have to guess. Attributes carry the same data
without inventing a schema nobody promised.
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

from homeassistant.components.sensor import SensorDeviceClass, SensorEntity
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import BrvgConfigEntry
from .entity import BrvgEntity


async def async_setup_entry(
    hass: HomeAssistant, entry: BrvgConfigEntry, async_add_entities: AddEntitiesCallback
) -> None:
    coordinator = entry.runtime_data
    entities: list[SensorEntity] = []
    for device in coordinator.devices:
        if not device.get("id"):
            continue
        entities.append(BrvgEventSensor(coordinator, device["id"]))
        entities.append(BrvgLastReportSensor(coordinator, device["id"]))
    async_add_entities(entities)


class BrvgEventSensor(BrvgEntity, SensorEntity):
    """The raw event name, with the cloud's classification and telemetry as attributes."""

    _attr_translation_key = "last_event"

    def __init__(self, coordinator, device_id: str) -> None:
        super().__init__(coordinator, device_id)
        vid = str((coordinator.data or {}).get("vid") or "")
        self._attr_unique_id = f"{vid}_{device_id}_event"

    @property
    def native_value(self) -> str | None:
        return self.device_data.get("event")

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        data = self.device_data
        return {
            "kind": data.get("kind"),
            "category": data.get("category"),
            **{f"telemetry_{k}": v for k, v in (data.get("extra") or {}).items()},
        }


class BrvgLastReportSensor(BrvgEntity, SensorEntity):
    """When this device last reported — the number that tells you a sensor has gone quiet."""

    _attr_translation_key = "last_report"
    _attr_device_class = SensorDeviceClass.TIMESTAMP

    def __init__(self, coordinator, device_id: str) -> None:
        super().__init__(coordinator, device_id)
        vid = str((coordinator.data or {}).get("vid") or "")
        self._attr_unique_id = f"{vid}_{device_id}_last_report"

    @property
    def native_value(self) -> datetime | None:
        at = self.device_data.get("at")
        if not isinstance(at, (int, float)) or at <= 0:
            return None
        # The wire carries UTC epoch ms, never a formatted local string — the platform's time policy.
        return datetime.fromtimestamp(at / 1000, tz=timezone.utc)
