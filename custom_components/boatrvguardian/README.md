# DockNeighbor for Home Assistant

Watch your boat or RV from the Home Assistant instance at your house — flood and bilge sensors,
shore power, batteries, temperature, switches — wherever the vessel actually is.

This integration talks to the DockNeighbor **cloud**. Home Assistant does not need to be
aboard, does not join the vessel's network, and does not talk to the sensors directly. That is the
point: the boat is usually somewhere you are not.

## Requirements

- Home Assistant 2025.2 or newer
- A DockNeighbor account with a vehicle on **any paid plan** (Dockside, the free plan, does not
  include integrations)
- An integration token, created in the app

## Install

**HACS** → Integrations → ⋮ → Custom repositories → add `DockNeighbor/DockNeighbor-Hub` as an
*Integration*, then install **DockNeighbor** and restart Home Assistant.

Then **Settings → Devices & Services → Add Integration → DockNeighbor**, and paste your vehicle
id and integration token.

## What you get

For every device aboard:

| Entity | What it is |
| --- | --- |
| **Alarm** (`binary_sensor`) | On while that device is in alarm. Flood sensors present as moisture, shore power as plug, and so on |
| **Last event** (`sensor`) | The most recent event, with the reading itself in attributes |
| **Last report** (`sensor`) | When the device last spoke — the one to alert on when a sensor goes quiet |

A device that stops appearing goes **unavailable** rather than reading "clear". On a boat that
distinction matters: a silent bilge sensor must never look like a dry bilge.

## Controlling the water valve

What a token may do is chosen **when you create it**, in the app:

| Scope | What appears in Home Assistant |
| --- | --- |
| Status only | Sensors. Nothing that changes anything |
| Status + close | A **Close water valve** button |
| Status + open and close | The button, plus the `boatrvguardian.open_valve` action |

If your token is status-only, no valve control appears at all — rather than a button that would be
refused. Widening it means creating a **new** token: the old one loses the power instead of gaining
it, so a token you gave to something you no longer trust cannot quietly become more powerful.

Opening always carries a time limit. There is no "open indefinitely", by design, and the vehicle's
owner may have set a maximum shorter than what you ask for.

```yaml
action: boatrvguardian.open_valve
data:
  duration_minutes: 20
```

**Closing is the safe direction and is treated that way throughout** — it works on any paid plan,
nothing rate-limits it, and if it fails you get an error rather than silence. A close that failed
quietly is the worst outcome there is here: you would walk away believing the water was off.

## How often it polls

It doesn't ask you, and it doesn't guess. Your plan has a telemetry resolution, the cloud reports it
on every response, and this integration polls at exactly that rate. Polling faster would return the
same data — so if you see it slow down, it is following your plan rather than struggling.

## Instant alerts

Polling is for state. For an alarm that reaches you in seconds, add a **webhook** destination in the
app pointed at a Home Assistant webhook, choose the *Home Assistant* template, and trigger on it:

```yaml
automation:
  - alias: Boat flood alarm
    trigger:
      - trigger: webhook
        webhook_id: your-webhook-id
        allowed_methods: [POST]
        local_only: false
    condition: "{{ trigger.json.state == 'alarm' and trigger.json.category == 'flood' }}"
    action:
      - action: notify.mobile_app_your_phone
        data:
          title: "{{ trigger.json.headline }}"
          message: "{{ trigger.json.body }}"
```

All-clears arrive the same way with `state == 'clear'`, so an automation you latch on an alarm can
unlatch itself.

## If the token stops working

Rotating or revoking a token in the app is the expected way this ends. Home Assistant will ask you to
paste the new one; nothing else needs reconfiguring.
