"""Shared entity base."""

from __future__ import annotations

from typing import Any

from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN
from .coordinator import BrvgCoordinator


class BrvgEntity(CoordinatorEntity[BrvgCoordinator]):
    """One sensor aboard one vehicle."""

    _attr_has_entity_name = True

    def __init__(self, coordinator: BrvgCoordinator, device_id: str) -> None:
        super().__init__(coordinator)
        self._device_id = device_id
        vid = str((coordinator.data or {}).get("vid") or "")
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, f"{vid}:{device_id}")},
            name=device_id,
            manufacturer="Boat & RV Guardian",
            model=str((coordinator.data or {}).get("vehicleType") or "vehicle"),
            via_device=(DOMAIN, vid),
        )

    @property
    def device_data(self) -> dict[str, Any]:
        return self.coordinator.device(self._device_id) or {}

    @property
    def available(self) -> bool:
        # A device that has dropped out of the vehicle payload is not "off", it is unknown — and on a
        # boat that distinction is the whole point: a silent bilge sensor must never read as "dry".
        return super().available and self.coordinator.device(self._device_id) is not None
