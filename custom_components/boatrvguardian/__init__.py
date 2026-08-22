"""The Boat & RV Guardian integration."""

from __future__ import annotations

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError
from homeassistant.helpers import config_validation as cv
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.aiohttp_client import async_get_clientsession
import voluptuous as vol

from .api import BrvgClient, BrvgError
from .const import (
    ATTR_DURATION_MINUTES,
    ATTR_ENTRY_ID,
    CONF_HOST,
    CONF_TOKEN,
    CONF_VID,
    DEFAULT_HOST,
    DOMAIN,
    SCOPE_CONTROL,
    SERVICE_OPEN_VALVE,
)
from .coordinator import BrvgCoordinator

OPEN_VALVE_SCHEMA = vol.Schema(
    {
        vol.Optional(ATTR_ENTRY_ID): cv.string,
        # REQUIRED in the schema, never defaulted in code. The valve may not run unbounded, and a
        # default here would be this component quietly choosing a limit that belongs to the owner.
        vol.Required(ATTR_DURATION_MINUTES): vol.All(vol.Coerce(int), vol.Range(min=1, max=1439)),
    }
)

PLATFORMS: list[Platform] = [Platform.BINARY_SENSOR, Platform.BUTTON, Platform.SENSOR]

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
    _async_register_services(hass)
    return True


def _async_register_services(hass: HomeAssistant) -> None:
    """Register `open_valve` once, however many vehicles are configured."""
    if hass.services.has_service(DOMAIN, SERVICE_OPEN_VALVE):
        return

    async def _open_valve(call) -> None:
        entry = _resolve_entry(hass, call.data.get(ATTR_ENTRY_ID))
        coordinator: BrvgCoordinator = entry.runtime_data
        data = coordinator.data or {}
        # Checked here as WELL as server-side, so the refusal names the real fix. Left to the cloud,
        # the user gets a 403 they have to interpret; checked here, they are told to mint a token
        # that can open — which is the thing they actually have to go and do.
        if data.get("scope") != SCOPE_CONTROL:
            raise ServiceValidationError(
                "This vehicle's token may not open the valve. Create a new token with the "
                '"open and close" scope in the Boat & RV Guardian app.'
            )
        if not (data.get("valve") or {}).get("present"):
            raise ServiceValidationError("This vehicle has no valve to open.")
        try:
            await coordinator.client.async_control_valve(
                "open", duration_minutes=call.data[ATTR_DURATION_MINUTES]
            )
        except BrvgError as err:
            raise HomeAssistantError(f"Could not open the valve: {err}") from err
        await coordinator.async_request_refresh()

    hass.services.async_register(DOMAIN, SERVICE_OPEN_VALVE, _open_valve, schema=OPEN_VALVE_SCHEMA)


def _resolve_entry(hass: HomeAssistant, entry_id: str | None) -> BrvgConfigEntry:
    """The vehicle a service call is about.

    Omitting it is allowed ONLY when exactly one vehicle is configured. With several, guessing which
    boat to put water into is not a convenience — so it is an error that names the ambiguity.
    """
    entries = list(hass.config_entries.async_loaded_entries(DOMAIN))
    if entry_id:
        for entry in entries:
            if entry.entry_id == entry_id:
                return entry
        raise ServiceValidationError("That vehicle is not set up.")
    if len(entries) != 1:
        raise ServiceValidationError(
            "Several vehicles are set up — say which one this call is for."
        )
    return entries[0]


async def async_unload_entry(hass: HomeAssistant, entry: BrvgConfigEntry) -> bool:
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
