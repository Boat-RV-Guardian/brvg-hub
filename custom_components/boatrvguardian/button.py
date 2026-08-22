"""Close the water valve — a button, deliberately, not a valve entity.

Home Assistant's ValveEntity wants to report whether the valve is open or closed, and the cloud read
API reports no valve STATE at all. A valve entity permanently answering "unknown" is worse than no
entity: on a dashboard it looks broken, and in an automation it invites conditions that can never be
true. A button says exactly what is true — this is a command you can send, not a state you can read.

Opening is NOT a button. An open must carry a bounded duration (the valve may never run unbounded),
and a button has nowhere to put one; a button that opened for some fixed number of minutes would be
this component inventing a limit that belongs to the owner. Opening is the `open_valve` action
instead, which takes the duration explicitly. See services.yaml.
"""

from __future__ import annotations

from homeassistant.components.button import ButtonEntity
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import BrvgConfigEntry
from .api import BrvgError
from .const import SCOPE_CONTROL, SCOPE_SAFE
from .entity import BrvgVehicleEntity


async def async_setup_entry(
    hass: HomeAssistant, entry: BrvgConfigEntry, async_add_entities: AddEntitiesCallback
) -> None:
    """Add the close button only when this vehicle has a valve AND this token may close it.

    Both halves matter. Without the valve check the button appears on a vehicle with no valve and
    fails in a way that reads as a permission problem; without the scope check a read-only token
    renders a control that is guaranteed to 403.
    """
    coordinator = entry.runtime_data
    data = coordinator.data or {}
    has_valve = bool((data.get("valve") or {}).get("present"))
    may_close = data.get("scope") in (SCOPE_SAFE, SCOPE_CONTROL)
    if has_valve and may_close:
        async_add_entities([BrvgCloseValveButton(coordinator)])


class BrvgCloseValveButton(BrvgVehicleEntity, ButtonEntity):
    """Shut the water off. The direction that can only prevent damage."""

    _attr_translation_key = "close_valve"

    def __init__(self, coordinator) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{self._vid}_close_valve"

    async def async_press(self) -> None:
        try:
            await self.coordinator.client.async_control_valve("close")
        except BrvgError as err:
            # Surfaced rather than swallowed: a close that silently failed is the worst possible
            # outcome here, because the person walks away believing the water is off.
            raise HomeAssistantError(f"Could not close the valve: {err}") from err
