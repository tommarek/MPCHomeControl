#!/usr/bin/env python3
"""A minimal MPC controller in ~40 lines of Python — proof the protocol is language-agnostic.

It subscribes to its command topic, honours the `valid_until` deadman, and (here) just *logs* the
command it would translate to hardware. Replace `translate()` with your real device protocol.

Dependency: `pip install paho-mqtt`. Run: `python3 python_controller_stub.py growatt`
"""
import json
import sys
import time
from datetime import datetime, timezone

import paho.mqtt.client as mqtt  # type: ignore

SCHEMA_MAJOR = "1"
ARMED = False  # never actuate from this stub

controller_id = sys.argv[1] if len(sys.argv) > 1 else "growatt"
last_seq = -1


def translate(cmd: dict) -> list[str]:
    """Map a command payload to your hardware messages (here: human-readable strings)."""
    p = cmd["payload"]
    if p["kind"] == "battery":
        return [f"set inverter slot={p['slot']} charge={p['charge_kw']}kW export={p['export_enabled']}"]
    if p["kind"] == "heating":
        return [f"zone {z['zone']} -> {'on' if z['on'] else 'off'}" for z in p["zones"]]
    return [f"(unhandled kind {p['kind']})"]


def on_message(client, _userdata, msg):
    global last_seq
    cmd = json.loads(msg.payload)
    now = datetime.now(timezone.utc)
    # The same four gates every controller applies (see ControlCommand::accept):
    if cmd["schema_version"].split(".")[0] != SCHEMA_MAJOR:
        return print(f"[{controller_id}] refuse: schema {cmd['schema_version']}")
    if cmd["controller_id"] != controller_id:
        return
    if cmd["command_seq"] <= last_seq:
        return print(f"[{controller_id}] skip stale seq {cmd['command_seq']}")
    if now >= datetime.fromisoformat(cmd["valid_until"]):
        return print(f"[{controller_id}] skip: past valid_until (deadman)")
    last_seq = cmd["command_seq"]
    for action in translate(cmd):
        print(f"[{controller_id}] {'SEND' if ARMED else 'would-send'}: {action}")


client = mqtt.Client(client_id=f"mpc-stub-{controller_id}")
client.will_set(f"mpc/health/{controller_id}", "offline", qos=1, retain=True)
client.on_message = on_message
client.connect("127.0.0.1", 1883)
client.publish(f"mpc/health/{controller_id}", "online", qos=1, retain=True)
client.subscribe(f"mpc/control/{controller_id}", qos=1)
print(f"[{controller_id}] listening on mpc/control/{controller_id} ({'ARMED' if ARMED else 'dry-run'})")
client.loop_forever()
