"""Thin client for the Boat & RV Guardian cloud read API.

Home Assistant talks to the CLOUD and to nothing else (owner ruling, 2026-08-20): it sits at a house
or an office watching a REMOTE vehicle, and never touches the boat's network, its hub, or its
sensors. So this module is the whole surface — there is no local discovery to fall back on, which is
why its error handling has to be specific rather than "something went wrong".
"""

from __future__ import annotations

import logging
from typing import Any

import aiohttp

_LOGGER = logging.getLogger(__name__)


class BrvgError(Exception):
    """Base error."""


class BrvgAuthError(BrvgError):
    """The token is missing, wrong, or revoked. Home Assistant should ask for a new one."""


class BrvgPlanError(BrvgError):
    """The vehicle's plan has no integration — Dockside, or a downgrade since setup."""


class BrvgRateLimited(BrvgError):
    """Polled faster than the plan's telemetry cadence. Carries the server's Retry-After."""

    def __init__(self, retry_after: int) -> None:
        super().__init__(f"rate limited, retry in {retry_after}s")
        self.retry_after = retry_after


class BrvgClient:
    """Reads one vehicle. One client per config entry, because one token reads one vehicle."""

    def __init__(self, session: aiohttp.ClientSession, host: str, vid: str, token: str) -> None:
        self._session = session
        self._host = host.rstrip("/")
        self._vid = vid
        self._token = token

    async def _get(self, path: str, params: dict[str, str] | None = None) -> dict[str, Any]:
        query = {"vid": self._vid, **(params or {})}
        try:
            async with self._session.get(
                f"{self._host}{path}",
                params=query,
                headers={"Authorization": f"Bearer {self._token}"},
                timeout=aiohttp.ClientTimeout(total=30),
            ) as res:
                # 401 covers both a wrong token and an unknown vehicle: the API refuses to
                # distinguish them so it cannot be used to enumerate vehicle ids. For us they mean
                # the same thing anyway — this entry can no longer read, so ask the user.
                if res.status == 401:
                    raise BrvgAuthError("token rejected (revoked, rotated, or wrong vehicle)")
                if res.status == 403:
                    raise BrvgPlanError("this plan does not include an integration")
                if res.status == 429:
                    raise BrvgRateLimited(int(res.headers.get("Retry-After", "60") or 60))
                res.raise_for_status()
                return await res.json()
        except TimeoutError as err:
            # asyncio.TimeoutError is the builtin TimeoutError from 3.11, and it is NOT an
            # aiohttp.ClientError — so the timeout set above would otherwise escape this client
            # entirely and surface as an unhandled exception in the coordinator. A slow cloud is the
            # single most likely failure this integration sees; it must be an ordinary UpdateFailed.
            raise BrvgError(f"timed out reaching {self._host}") from err
        except aiohttp.ClientError as err:
            raise BrvgError(f"cannot reach {self._host}: {err}") from err

    async def async_get_vehicle(self) -> dict[str, Any]:
        """Current state of every device that has ever reported."""
        return await self._get("/api/v1/vehicle")

    # There is deliberately NO history client here. `GET /api/v1/history` exists in the cloud
    # API and serves scripts and dashboards, but this component has no use for it: Home
    # Assistant records its own history from install, and there is no numeric entity here to
    # backfill — the vendor-shaped `extra` map is exposed as attributes rather than entities
    # precisely because its units are unknowable. A wrapper with no caller but its own tests is
    # worse than plain dead code, because the tests make it look exercised.
