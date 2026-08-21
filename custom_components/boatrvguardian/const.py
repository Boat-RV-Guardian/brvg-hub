"""Constants for the Boat & RV Guardian integration.

The vocabulary here is SERVED by the cloud, not defined here: `kind` and `category` arrive on every
device in the API response precisely so this component never re-implements the worker's event
classifier. A second copy of those rules drifts from the first the moment a vendor spells an event
differently, which has happened twice in this platform's history. Keep it that way — if you find
yourself parsing an event NAME in this component, the fix belongs in the API instead.
"""

from __future__ import annotations

from typing import Final

DOMAIN: Final = "boatrvguardian"

CONF_VID: Final = "vid"
CONF_TOKEN: Final = "token"
CONF_HOST: Final = "host"

DEFAULT_HOST: Final = "https://api.boatrvguardian.com"

# The cloud reports the plan's telemetry resolution on every response, and polling faster returns
# identical data — so the coordinator follows what the server says rather than a number chosen here.
# These only bound it: never hammer, never appear frozen.
MIN_POLL_SECONDS: Final = 30
MAX_POLL_SECONDS: Final = 900

# Alarm categories the cloud can report. Mapped to a device class where Home Assistant has one that
# genuinely matches; everything else is a PROBLEM, which is honest rather than decorative.
CATEGORY_FLOOD: Final = "flood"
CATEGORY_SHORE_POWER: Final = "shore_power"
CATEGORY_BATTERY: Final = "battery"
CATEGORY_CLIMATE: Final = "climate"
CATEGORY_SECURITY: Final = "security"
CATEGORY_PRESENCE: Final = "presence"

# `kind` values, straight from the API.
KIND_ALARM: Final = "alarm"
KIND_CLEAR: Final = "clear"
KIND_TELEMETRY: Final = "telemetry"
KIND_STATE: Final = "state"
