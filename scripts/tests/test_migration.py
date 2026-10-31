import importlib.util
import json
from pathlib import Path
import sys
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
spec = importlib.util.spec_from_file_location('migrate_kuma', ROOT / 'migrate-kuma.py')
migration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(migration)
from jsonata_to_jq import TranslationError, translate


def fixture():
    rows = []
    for i, kind in enumerate(['http', 'keyword', 'json-query', 'mqtt', 'push', 'port', 'ping', 'dns'], 1):
        row = dict(id=i, name=f'Monitor {i}', type=kind, active=1, interval=60, timeout=10,
                   maxretries=2, retry_interval=15, resend_interval=3, group='Example',
                   description='Synthetic description', notifications=['quiet'])
        if kind in ('http', 'keyword', 'json-query'):
            row.update(url='https://example.invalid/health?token=synthetic-secret', headers={'Authorization': 'Bearer synthetic-secret'}, accepted_statuscodes_json=['200-299'])
        if kind == 'keyword': row['keyword'] = 'ready'
        if kind == 'json-query': row.update(json_path='$ > 100', expected_value='true')
        if kind == 'mqtt': row.update(hostname='broker.invalid', port=1883, mqtt_username='reader', mqtt_password='synthetic-secret', mqtt_topic='sensor/state', mqtt_check_type='keyword', mqtt_success_message='online')
        if kind == 'push': row['push_token'] = 'synthetic-secret'
        if kind in ('port', 'ping', 'dns'): row.update(hostname='service.invalid', port=443)
        if kind == 'ping': row.update(ping_count=3, packet_size=56)
        if kind == 'dns': row.update(dns_resolve_type='A', dns_resolve_server='127.0.0.1')
        rows.append(row)
    return dict(settings={'keepDataPeriodDays': '30'}, monitors=rows,
                notification_channels=[dict(name='quiet', type='pushover', pushover_priority=-1, pushover_sound='none')],
                maintenance_windows=[dict(title='Example', active=True, strategy='cron', cron='0 3 * * *', timezone='Europe/Berlin', duration=900, monitors=['Monitor 1'])])


class MigrationTest(unittest.TestCase):
    def test_redaction_suffixes_and_url_encoded_markers(self):
        for marker in ['<REDACTED>', '<REDACTED-UUID>', '<redacted_API_KEY>',
                       '%3CREDACTED%3E', '%3cREDACTED-UUID%3e']:
            with self.subTest(marker=marker):
                data = fixture()
                data['monitors'][0]['url'] = f'https://example.invalid/{marker}?q={marker}&ok=yes'
                config, report = migration.Converter(data).convert()
                self.assertEqual(report['unmapped'], [])
                self.assertEqual(config['monitors'][0]['target'],
                                 'https://example.invalid/${HORA_MONITOR_1_SECRET_1}?q=${HORA_MONITOR_1_SECRET_2}&ok=yes')
                self.assertIn('HORA_MONITOR_1_SECRET_1', report['required_environment'])
                self.assertIn('HORA_MONITOR_1_SECRET_2', report['required_environment'])

    def test_unresolved_markers_stop_cli_without_replacing_existing_config(self):
        for marker in ['<REDACTED-UUID', '%3CREDACTED-UUID']:
            data = fixture()
            data['monitors'][0]['url'] = 'https://example.invalid/' + marker
            with tempfile.TemporaryDirectory() as directory:
                directory = Path(directory)
                source = directory / 'export.json'
                source.write_text(json.dumps(data))
                existing = directory / 'config.toml'
                existing.write_text('existing private config')
                result = subprocess.run([sys.executable, str(ROOT / 'migrate-kuma.py'),
                                         str(source), '--output-dir', str(directory)],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(existing.read_text(), 'existing private config')
                self.assertNotIn(marker, result.stdout + result.stderr)
                self.assertTrue(json.loads((directory / 'report.json').read_text())['unmapped'])

    def test_watchdog_requires_separate_url_and_preserves_other_monitors(self):
        data = fixture()
        data['monitors'][0]['headers'] = {}
        data['monitors'][0]['url'] = 'https://example.invalid/old-kuma-check'
        data['maintenance_windows'][0]['monitors'] = ['Monitor 2']
        baseline, _ = migration.Converter(data).convert()
        config, report = migration.Converter(data, watchdog_id=1).convert()
        self.assertEqual(report['unmapped'], [])
        self.assertEqual(report['watchdogs'], 1)
        self.assertEqual(report['monitors'], 7)
        self.assertEqual(config['monitors'], baseline['monitors'][1:])
        self.assertEqual(config['health'], dict(id='hora', interval_secs=60,
                                              heartbeat_url='${HORA_HEARTBEAT_URL}'))
        self.assertNotIn('old-kuma-check', migration.render(config) + json.dumps(report))
        self.assertIn('HORA_HEARTBEAT_URL', report['required_environment'])

    def test_invalid_watchdog_or_scoped_maintenance_is_reported(self):
        data = fixture()
        for mid in [999, 4, 1]:  # absent, MQTT, custom headers + maintenance
            self.assertTrue(migration.Converter(data, watchdog_id=mid).convert()[1]['unmapped'])
        data['monitors'][0]['headers'] = {}
        data['maintenance_windows'][0]['monitors'] = []  # all monitors
        self.assertTrue(migration.Converter(data, watchdog_id=1).convert()[1]['unmapped'])

    def test_complete_mapping_and_secret_extraction(self):
        config, report = migration.Converter(fixture(), {'4': 300}).convert()
        self.assertEqual(report['unmapped'], [])
        self.assertEqual(len(config['monitors']), 8)
        self.assertEqual(config['maintenance'][0]['monitors'], ['kuma-1'])
        text = migration.render(config)
        self.assertNotIn('synthetic-secret', text)
        self.assertNotIn('synthetic-secret', json.dumps(report))
        for monitor in config['monitors']:
            self.assertEqual(monitor['fail_threshold'], 3)
            self.assertEqual(monitor['repeat_alert_secs'], 180)
            self.assertEqual(monitor['notify'], ['quiet'])
            self.assertFalse(monitor['public'])
        self.assertEqual(config['monitors'][3]['max_age_secs'], 300)
        self.assertEqual(config['monitors'][4]['push_poll_secs'], 30)

    def test_shared_broker_and_retained_topic(self):
        data = fixture()
        second = dict(data['monitors'][3], id=9, name='Second MQTT', mqtt_topic='other/state')
        data['monitors'].append(second)
        config, report = migration.Converter(data).convert()
        self.assertEqual(report['brokers'], 1)
        self.assertNotIn('max_age_secs', config['monitors'][3])
        self.assertEqual(config['monitors'][3]['broker'], config['monitors'][-1]['broker'])

    def test_unknown_values_fail_explicitly(self):
        for key, value in [('unknownOption', True), ('type', 'unsupported'), ('active', 0)]:
            data = fixture()
            data['monitors'][0][key] = value
            self.assertTrue(migration.Converter(data).convert()[1]['unmapped'])
        data = fixture()
        data['monitors'][2]['json_path'] = '$eval("secret")'
        self.assertTrue(migration.Converter(data).convert()[1]['unmapped'])

    def test_unsupported_jsonata_is_rejected(self):
        for source in ['$eval("x")', '$map($, function($v){$v})', '$round($, 99)', '$a := 1', '$unbound', '($__a := 1; $__a)']:
            with self.assertRaises((TranslationError, IndexError)):
                translate(source)


if __name__ == '__main__':
    unittest.main()
