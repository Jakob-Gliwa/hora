#!/usr/bin/env python3
"""Convert a normalized Uptime Kuma monitor export without exposing credentials.

Writes a private config, environment template and explicit unmapped-field report.
Python 3.9+, standard library only. See docs/kuma-migration.md.
"""
import argparse
import json
import os
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit, urlunsplit

from jsonata_to_jq import TranslationError, translate

MONITOR_FIELDS = set("id name type active description group notifications interval timeout maxretries retry_interval resend_interval url method headers ignore_tls accepted_statuscodes_json keyword keyword_invert invert_keyword hostname port dns_resolve_server dns_resolve_type expected_value json_path json_path_operator mqtt_check_type mqtt_password mqtt_username mqtt_success_message mqtt_topic push_token ping_count packet_size".split())
CHANNEL_FIELDS = set("name type is_default pushover_priority pushover_sound".split())
MAINTENANCE_FIELDS = set("active cron description duration monitors strategy timezone title".split())
SECRET_KEY = re.compile(r"token|api[_-]?key|password|secret|authorization|cookie", re.I)
REDACTED = re.compile(r"<REDACTED>|%3CREDACTED%3E", re.I)


def toml_value(value):
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{json.dumps(k)} = {toml_value(v)}" for k, v in value.items()) + " }"
    if isinstance(value, list):
        return "[" + ", ".join(map(toml_value, value)) + "]"
    if value is None:
        raise ValueError("TOML cannot encode null")
    return json.dumps(value, ensure_ascii=False)


def render(config):
    lines = ["# Private generated configuration. Keep this file and the export out of Git."]
    for section, value in config.items():
        rows = value if isinstance(value, list) else [value]
        for row in rows:
            lines.append(f"\n[[{section}]]" if isinstance(value, list) else f"\n[{section}]")
            lines.extend(f"{key} = {toml_value(val)}" for key, val in row.items())
    return "\n".join(lines) + "\n"


class Converter:
    def __init__(self, data, freshness=None):
        self.data = data
        self.freshness = freshness or {}
        self.env = {}
        self.issues = []
        self.decisions = []
        self.brokers = {}
        self.monitors = data.get("monitors", [])
        self.by_id = {str(m["id"]): f"kuma-{m['id']}" for m in self.monitors}
        self.by_name = {}
        for m in self.monitors:
            self.by_name.setdefault(m["name"], []).append(self.by_id[str(m["id"])])

    def placeholder(self, name, purpose):
        name = re.sub(r"[^A-Z0-9_]", "_", name.upper())
        self.env[name] = purpose
        return "${" + name + "}"

    def issue(self, owner, message):
        self.issues.append({"owner": owner, "reason": message})

    def unknown(self, owner, data, allowed):
        for key in sorted(set(data) - allowed):
            self.issue(owner, f"unsupported field: {key}")

    def text_secrets(self, value, prefix):
        counter = 0
        def replace(_match):
            nonlocal counter
            counter += 1
            return self.placeholder(f"{prefix}_{counter}", "redacted value")
        value = REDACTED.sub(replace, str(value))
        for name in re.findall(r"\$\{([A-Za-z_][A-Za-z_0-9]*)\}", value):
            self.env.setdefault(name, "existing environment reference")
        return value

    def url(self, value, prefix):
        parts = urlsplit(value)
        netloc = parts.netloc
        if "@" in netloc:
            credentials, host = netloc.rsplit("@", 1)
            user, colon, _password = credentials.partition(":")
            user = self.placeholder(prefix + "_USER", "URL username")
            password = self.placeholder(prefix + "_PASSWORD", "URL password") if colon else ""
            netloc = user + (":" + password if colon else "") + "@" + host
        query = []
        for item in parts.query.split("&") if parts.query else []:
            key, sep, val = item.partition("=")
            if SECRET_KEY.search(unquote(key)) and val:
                val = self.placeholder(prefix + "_" + unquote(key), "URL query credential (URL-encode the value)")
            query.append(key + sep + val)
        return self.text_secrets(urlunsplit((parts.scheme, netloc, parts.path, "&".join(query), parts.fragment)), prefix + "_SECRET")

    def channels(self):
        result = []
        for i, channel in enumerate(self.data.get("notification_channels", [])):
            owner = f"channel {i + 1}"
            self.unknown(owner, channel, CHANNEL_FIELDS)
            if channel.get("type") != "pushover":
                self.issue(owner, "only Pushover channels are supported")
                continue
            row = {"name": channel["name"], "type": "pushover", "token": self.placeholder("PUSHOVER_TOKEN", "Pushover application API token"), "user": self.placeholder("PUSHOVER_USER", "Pushover recipient key")}
            if "pushover_priority" in channel:
                priority = int(channel["pushover_priority"])
                if not -2 <= priority <= 1:
                    self.issue(owner, "emergency priority needs explicit retry/expire settings")
                row["priority"] = priority
            if channel.get("pushover_sound"):
                row["sound"] = channel["pushover_sound"]
            result.append(row)
        return result

    def broker(self, monitor):
        host = monitor["hostname"]
        port = int(monitor.get("port") or 1883)
        username = monitor.get("mqtt_username")
        identity = (host, port, username, monitor.get("mqtt_password"))
        if identity not in self.brokers:
            number = len(self.brokers) + 1
            host = f"[{host}]" if ":" in host and not host.startswith("[") else host
            row = {"name": f"mqtt-{number}", "url": f"mqtt://{host}:{port}"}
            if username:
                row["username"] = self.text_secrets(username, f"HORA_MQTT_USER_{number}")
            if monitor.get("mqtt_password"):
                row["password"] = self.placeholder(f"HORA_MQTT_PASSWORD_{number}", "MQTT broker password")
            self.brokers[identity] = row
        return self.brokers[identity]["name"]

    def assertion(self, monitor, row):
        try:
            row["jq"] = translate(monitor["json_path"])
        except (TranslationError, KeyError, IndexError):
            self.issue(row["id"], "JSONata expression is outside the supported translation subset")
            return
        row["jq_expected"] = str(monitor.get("expected_value", "true"))
        operator = monitor.get("json_path_operator") or "=="
        if operator not in ("==", "!=", "<", "<=", ">", ">="):
            self.issue(row["id"], "unsupported JSON comparison operator")
        row["jq_operator"] = operator

    def monitor(self, monitor):
        mid = str(monitor["id"])
        owner = self.by_id[mid]
        self.unknown(owner, monitor, MONITOR_FIELDS)
        if not monitor.get("active", 1):
            self.issue(owner, "inactive monitors need an explicit enable/omit decision")
        interval = int(monitor["interval"])
        if interval <= 0:
            raise ValueError("monitor interval must be positive")
        row = {"id": owner, "name": monitor["name"], "interval_secs": interval,
               "fail_threshold": int(monitor.get("maxretries", 0)) + 1,
               "probe_retries": 0, "repeat_alert_secs": int(monitor.get("resend_interval", 0)) * interval,
               "public": False}
        if monitor.get("group"):
            row["group"] = monitor["group"]
        if monitor.get("description"):
            row["description"] = monitor["description"]
        defaults = [c["name"] for c in self.data.get("notification_channels", []) if c.get("is_default")]
        row["notify"] = monitor.get("notifications", defaults)
        if int(monitor.get("retry_interval") or interval) > 0:
            row["retry_interval_secs"] = int(monitor.get("retry_interval") or interval)
        timeout = int(monitor.get("timeout") or 0)
        if timeout > 0:
            row["timeout_secs"] = timeout
        kind = monitor["type"]
        if kind in ("http", "keyword", "json-query"):
            row.update(kind="http", target=self.url(monitor["url"], f"HORA_MONITOR_{mid}"))
            if monitor.get("method"):
                row["method"] = monitor["method"]
            if monitor.get("ignore_tls"):
                row.update(ignore_tls=True, check_cert=False)
            headers = monitor.get("headers") or {}
            if isinstance(headers, str):
                headers = json.loads(headers)
            if headers:
                row["headers"] = {k: self.placeholder(f"HORA_MONITOR_{mid}_HEADER_{k}", "HTTP header value") if SECRET_KEY.search(k) else self.text_secrets(v, f"HORA_MONITOR_{mid}_HEADER_{k}") for k, v in headers.items()}
            statuses = monitor.get("accepted_statuscodes_json")
            if statuses:
                statuses = json.loads(statuses) if isinstance(statuses, str) else statuses
                codes = set()
                for status in statuses:
                    bounds = str(status).split("-")
                    lo, hi = int(bounds[0]), int(bounds[-1])
                    if not 100 <= lo <= hi <= 599:
                        raise ValueError("invalid accepted HTTP status range")
                    codes.update(range(lo, hi + 1))
                row["expected_statuses"] = sorted(codes)
            else:
                # Kuma defaults to 200..299; Hora's default is identical.
                pass
            if kind == "keyword":
                row["keyword"] = monitor["keyword"]
                row["keyword_invert"] = bool(monitor.get("keyword_invert", monitor.get("invert_keyword", False)))
            if kind == "json-query":
                self.assertion(monitor, row)
        elif kind == "mqtt":
            row.update(kind="mqtt", broker=self.broker(monitor), topic=monitor["mqtt_topic"])
            if mid in self.freshness:
                row["max_age_secs"] = self.freshness[mid]
            check = monitor.get("mqtt_check_type", "keyword")
            if check == "keyword":
                row["keyword"] = monitor["mqtt_success_message"]
                row["keyword_invert"] = bool(monitor.get("keyword_invert", monitor.get("invert_keyword", False)))
            elif check == "json-query":
                self.assertion(monitor, row)
            else:
                self.issue(owner, "unsupported MQTT assertion type")
        elif kind == "push":
            row.update(kind="push", push_token=self.placeholder(f"HORA_PUSH_TOKEN_{mid}", "existing Kuma push token"), kuma_push=True, push_poll_secs=min(30, interval))
        elif kind == "port":
            host = monitor["hostname"]
            host = f"[{host}]" if ":" in host and not host.startswith("[") else host
            row.update(kind="tcp", target=f"{host}:{int(monitor['port'])}")
        elif kind == "ping":
            row.update(kind="icmp", target=monitor["hostname"])
            for key in ("ping_count", "packet_size"):
                if monitor.get(key): row[key] = int(monitor[key])
        elif kind == "dns":
            row.update(kind="dns", target=monitor["hostname"], dns_record=monitor.get("dns_resolve_type", "A"))
            if monitor.get("dns_resolve_server"):
                resolver = monitor["dns_resolve_server"]
                row["dns_resolver"] = f"[{resolver}]:53" if ":" in resolver else resolver + ":53"
            if monitor.get("expected_value"):
                row["dns_expected"] = monitor["expected_value"]
        else:
            self.issue(owner, "unsupported monitor type")
        return row

    def maintenance(self):
        rows = []
        for i, window in enumerate(self.data.get("maintenance_windows", [])):
            owner = f"maintenance {i + 1}"
            self.unknown(owner, window, MAINTENANCE_FIELDS)
            if not window.get("active", True):
                self.decisions.append({"owner": owner, "decision": "inactive window omitted"})
                continue
            if window.get("strategy") != "cron":
                self.issue(owner, "unsupported maintenance strategy")
                continue
            ids = []
            for ref in window.get("monitors", []):
                if str(ref) in self.by_id:
                    ids.append(self.by_id[str(ref)])
                elif len(self.by_name.get(str(ref), [])) == 1:
                    ids.extend(self.by_name[str(ref)])
                else:
                    self.issue(owner, "maintenance monitor reference missing or ambiguous")
            rows.append({"title": window["title"], "cron": window["cron"], "timezone": window.get("timezone", "UTC"), "duration_secs": int(window["duration"]), "monitors": ids})
        return rows

    def convert(self):
        self.unknown("export", self.data, {"source", "notes", "settings", "notification_channels", "maintenance_windows", "monitors"})
        self.unknown("settings", self.data.get("settings", {}), {"keepDataPeriodDays"})
        if len(self.by_id) != len(self.monitors):
            self.issue("export", "duplicate monitor IDs")
        for mid in self.freshness:
            if not any(str(m["id"]) == mid and m["type"] == "mqtt" for m in self.monitors):
                self.issue("freshness", "max-age override does not identify an MQTT monitor")
        channels = self.channels()
        monitors = [self.monitor(m) for m in self.monitors]
        config = {"page": {"title": "Service status"},
                  "server": {"bind": "127.0.0.1:8787", "database_path": "hora.db", "auth_token": self.placeholder("HORA_AUTH_TOKEN", "operator/viewer token (generate a new random value)")},
                  "alerts": {"fail_threshold": 3, "default_retention_days": int(self.data.get("settings", {}).get("keepDataPeriodDays", 90))},
                  "updates": {"check": False}, "channels": channels, "brokers": list(self.brokers.values()),
                  "maintenance": self.maintenance(), "monitors": monitors}
        channel_names = {c["name"] for c in channels}
        for m in monitors:
            if any(name not in channel_names for name in m["notify"]):
                self.issue(m["id"], "notification route references an unknown channel")
        report = {"monitors": len(monitors), "groups": len({m.get("group") for m in monitors}),
                  "channels": len(channels), "maintenance_windows": len(config["maintenance"]),
                  "brokers": len(self.brokers), "freshness_overrides": len(self.freshness),
                  "required_environment": self.env, "decisions": self.decisions, "unmapped": self.issues}
        return config, report


def private_write(path, content):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        os.fchmod(output.fileno(), 0o600)
        output.write(content)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("export", type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path(".local/migration"))
    parser.add_argument("--mqtt-max-age", action="append", default=[], metavar="ID=SECONDS", help="freshness deadline for a known periodic topic; repeatable")
    args = parser.parse_args()
    try:
        freshness = {}
        for entry in args.mqtt_max_age:
            mid, seconds = entry.split("=", 1)
            seconds = int(seconds)
            if not 0 < seconds <= 2592000: raise ValueError("invalid MQTT max age")
            freshness[mid] = seconds
        data = json.loads(args.export.read_text(encoding="utf-8"))
        converter = Converter(data, freshness)
        config, report = converter.convert()
        args.output_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        config["server"]["database_path"] = str((args.output_dir / "hora.db").resolve())
        private_write(args.output_dir / "report.json", json.dumps(report, indent=2, ensure_ascii=False) + "\n")
        if report["unmapped"]:
            print(f"Migration stopped: {len(report['unmapped'])} unmapped items. See the local report.", file=sys.stderr)
            return 1
        private_write(args.output_dir / "config.toml", render(config))
        private_write(args.output_dir / "environment.example", "# Fill these locally; never commit credentials.\n" + "".join(f"# {purpose}\n{name}=\n" for name, purpose in sorted(converter.env.items())))
        print(f"Converted {report['monitors']} monitors, {report['channels']} channels and {report['maintenance_windows']} maintenance windows; unmapped: 0.")
        print("Private output written. Set the environment values before running hora check.")
        return 0
    except (ValueError, KeyError, TypeError, OSError, IndexError) as error:
        # Source values and exception payloads may contain credentials.
        print(f"Migration failed ({type(error).__name__}); no source values were printed.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
