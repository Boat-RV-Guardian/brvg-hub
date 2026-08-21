"""Config flow: host, vehicle id, token — validated by actually reading the vehicle."""

from __future__ import annotations

from typing import Any

from homeassistant.config_entries import ConfigFlow, ConfigFlowResult
from homeassistant.helpers.aiohttp_client import async_get_clientsession
import voluptuous as vol

from .api import BrvgAuthError, BrvgClient, BrvgError, BrvgPlanError, BrvgRateLimited
from .const import CONF_HOST, CONF_TOKEN, CONF_VID, DEFAULT_HOST, DOMAIN


class BrvgConfigFlow(ConfigFlow, domain=DOMAIN):
    """Set up one vehicle. One token reads one vehicle, so one entry IS one vehicle."""

    VERSION = 1

    async def _validate(self, data: dict[str, Any]) -> tuple[dict[str, str], str | None]:
        """Return (errors, vehicle_name). Validation is a real read, not a shape check."""
        client = BrvgClient(
            async_get_clientsession(self.hass),
            data[CONF_HOST],
            data[CONF_VID],
            data[CONF_TOKEN],
        )
        try:
            vehicle = await client.async_get_vehicle()
        except BrvgAuthError:
            return {"base": "invalid_auth"}, None
        except BrvgPlanError:
            return {"base": "plan_excluded"}, None
        except BrvgRateLimited:
            # A retry a moment later succeeds; saying "cannot connect" here would send someone
            # hunting a network problem that does not exist.
            return {"base": "rate_limited"}, None
        except BrvgError:
            return {"base": "cannot_connect"}, None
        return {}, str(vehicle.get("name") or data[CONF_VID])

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            await self.async_set_unique_id(user_input[CONF_VID])
            self._abort_if_unique_id_configured()
            errors, name = await self._validate(user_input)
            if not errors:
                return self.async_create_entry(title=name or "", data=user_input)

        return self.async_show_form(
            step_id="user",
            data_schema=vol.Schema(
                {
                    vol.Required(CONF_VID): str,
                    vol.Required(CONF_TOKEN): str,
                    vol.Required(CONF_HOST, default=DEFAULT_HOST): str,
                }
            ),
            errors=errors,
        )

    async def async_step_reauth(self, entry_data: dict[str, Any]) -> ConfigFlowResult:
        """A rotated or revoked token is the expected way an entry stops working."""
        return await self.async_step_reauth_confirm()

    async def async_step_reauth_confirm(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        entry = self._get_reauth_entry()
        errors: dict[str, str] = {}
        if user_input is not None:
            candidate = {**entry.data, CONF_TOKEN: user_input[CONF_TOKEN]}
            errors, _ = await self._validate(candidate)
            if not errors:
                return self.async_update_reload_and_abort(entry, data=candidate)

        return self.async_show_form(
            step_id="reauth_confirm",
            data_schema=vol.Schema({vol.Required(CONF_TOKEN): str}),
            errors=errors,
        )
