"""The client's error mapping.

This is the whole reason the client has typed errors rather than one exception: Home Assistant has to
do something DIFFERENT for each of them — reauth for a rejected token, a plain failure for a
downgraded plan, and a back-off for "asked too soon", which is not a failure at all.
"""

from __future__ import annotations

from homeassistant.helpers.aiohttp_client import async_get_clientsession
import pytest

from custom_components.boatrvguardian.api import (
    BrvgAuthError,
    BrvgClient,
    BrvgError,
    BrvgPlanError,
    BrvgRateLimited,
)

from .conftest import HOST, TOKEN, VEHICLE_URL, VID, vehicle_payload


def client(hass) -> BrvgClient:
    return BrvgClient(async_get_clientsession(hass), HOST, VID, TOKEN)


async def test_reads_a_vehicle_and_sends_the_token(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, json=vehicle_payload())
    data = await client(hass).async_get_vehicle()
    assert data["vid"] == VID

    _method, url, _data, headers = aioclient_mock.mock_calls[0]
    assert headers["Authorization"] == f"Bearer {TOKEN}"
    assert url.query["vid"] == VID


async def test_401_is_an_auth_error(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, status=401, json={"error": "unauthorized"})
    with pytest.raises(BrvgAuthError):
        await client(hass).async_get_vehicle()


async def test_403_is_a_plan_error_not_an_auth_error(hass, aioclient_mock) -> None:
    """Retrying or re-entering the token never fixes a Dockside vehicle."""
    aioclient_mock.get(VEHICLE_URL, status=403, json={"error": "forbidden"})
    with pytest.raises(BrvgPlanError):
        await client(hass).async_get_vehicle()


async def test_429_carries_the_servers_retry_after(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, status=429, headers={"Retry-After": "300"}, json={})
    with pytest.raises(BrvgRateLimited) as err:
        await client(hass).async_get_vehicle()
    assert err.value.retry_after == 300


async def test_429_without_a_header_still_backs_off(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, status=429, json={})
    with pytest.raises(BrvgRateLimited) as err:
        await client(hass).async_get_vehicle()
    assert err.value.retry_after == 60


async def test_a_server_error_is_a_plain_error(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, status=500, json={})
    with pytest.raises(BrvgError):
        await client(hass).async_get_vehicle()


async def test_a_dead_host_is_an_error_not_a_crash(hass, aioclient_mock) -> None:
    aioclient_mock.get(VEHICLE_URL, exc=TimeoutError)
    with pytest.raises(BrvgError):
        await client(hass).async_get_vehicle()
