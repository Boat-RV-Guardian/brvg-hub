"""The Boat & RV Guardian integration."""

from __future__ import annotations

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .api import BrvgClient
from .const import CONF_HOST, CONF_TOKEN, CONF_VID, DEFAULT_HOST, DOMAIN
from .coordinator import BrvgCoordinator

PLATFORMS: list[Platform] = [Platform.BINARY_SENSOR, Platform.SENSOR]

type BrvgConfigEntry = ConfigEntry[BrvgCoordinator]


async def async_setup_entry(hass: HomeAssistant, entry: BrvgConfigEntry) -> bool:
    client = BrvgClient(
        async_get_clientsession(hass),
        entry.data.get(CONF_HOST, DEFAULT_HOST),
        entry.data[CONF_VID],
        entry.data[CONF_TOKEN],
    )
    coordinator = BrvgCoordinator(hass, entry, client)
    await coordinator.async_config_entry_first_refresh()
    entry.runtime_data = coordinator

    # The VEHICLE is the hub device and every sensor hangs off it via `via_device`. It has to be
    # registered explicitly: pointing `via_device` at a device nobody created logs a warning today
    # and stops working in HA 2025.12. It also gives the boat one card in the UI instead of a flat
    # list of sensors with no idea which vessel they are on.
    vehicle = coordinator.data or {}
    dr.async_get(hass).async_get_or_create(
        config_entry_id=entry.entry_id,
        identifiers={(DOMAIN, entry.data[CONF_VID])},
        name=str(vehicle.get("name") or entry.data[CONF_VID]),
        manufacturer="Boat & RV Guardian",
        model=str(vehicle.get("vehicleType") or "vehicle"),
    )
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: BrvgConfigEntry) -> bool:
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
