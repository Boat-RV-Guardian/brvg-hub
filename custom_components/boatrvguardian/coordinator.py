"""Polling coordinator.

The poll interval is NOT configured here or by the user: the cloud reports `resolutionSec` — the
telemetry resolution of the vehicle's plan — on every response, and the API rate-limits to exactly
that cadence. Polling faster returns identical data and earns a 429. So the server is the authority
on how often to ask, and this class follows it.
"""

from __future__ import annotations

from datetime import timedelta
import logging
from typing import Any

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .api import BrvgAuthError, BrvgClient, BrvgError, BrvgPlanError, BrvgRateLimited
from .const import DOMAIN, MAX_POLL_SECONDS, MIN_POLL_SECONDS

_LOGGER = logging.getLogger(__name__)


class BrvgCoordinator(DataUpdateCoordinator[dict[str, Any]]):
    """Fetches the vehicle, and re-paces itself to whatever the plan allows."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry, client: BrvgClient) -> None:
        super().__init__(
            hass,
            _LOGGER,
            name=DOMAIN,
            config_entry=entry,
            # Start at the coarsest cadence and speed UP once the plan is known. The other direction
            # would spend a real customer's first window on a 429.
            update_interval=timedelta(seconds=MAX_POLL_SECONDS),
        )
        self.client = client

    async def _async_update_data(self) -> dict[str, Any]:
        try:
            data = await self.client.async_get_vehicle()
        except BrvgAuthError as err:
            # Triggers Home Assistant's reauth flow rather than leaving entities unavailable with no
            # explanation. A revoked or rotated token is the expected way this ends.
            raise ConfigEntryAuthFailed(str(err)) from err
        except BrvgRateLimited as err:
            # Not a failure — we asked too soon. Back off to what the server said and keep the last
            # known state, which is still correct: nothing could have changed inside the window.
            self.update_interval = timedelta(seconds=max(err.retry_after, MIN_POLL_SECONDS))
            _LOGGER.debug("rate limited; backing off to %ss", self.update_interval.total_seconds())
            return self.data or {}
        except (BrvgPlanError, BrvgError) as err:
            raise UpdateFailed(str(err)) from err

        self._follow_plan_cadence(data)
        return data

    def _follow_plan_cadence(self, data: dict[str, Any]) -> None:
        """Match the plan's telemetry resolution, bounded so we neither hammer nor look frozen."""
        raw = data.get("resolutionSec")
        if not isinstance(raw, (int, float)) or raw <= 0:
            return
        seconds = min(max(int(raw), MIN_POLL_SECONDS), MAX_POLL_SECONDS)
        if self.update_interval != timedelta(seconds=seconds):
            _LOGGER.debug("following plan cadence: polling every %ss", seconds)
            self.update_interval = timedelta(seconds=seconds)

    @property
    def devices(self) -> list[dict[str, Any]]:
        return list((self.data or {}).get("devices") or [])

    def device(self, device_id: str) -> dict[str, Any] | None:
        return next((d for d in self.devices if d.get("id") == device_id), None)
