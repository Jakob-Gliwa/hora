#!/usr/bin/env python3
"""Remap a private normalized export to loopback fixtures for an offline benchmark.

All output stays private. Notifications, external requests and update checks are
removed. Expressions and probe cadence are preserved. Requires migration cases
with passing samples for JSON assertions (the private differential test output).
"""
import argparse
import base64
import importlib.util
import json
from pathlib import Path

spec = importlib.util.spec_from_file_location('migration', Path(__file__).with_name('migrate-kuma.py'))
migration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(migration)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('export', type=Path)
    parser.add_argument('--cases', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, default=Path('.local/benchmark'))
    parser.add_argument('--mqtt-max-age', action='append', default=[], metavar='ID=SECONDS')
    args = parser.parse_args()
    source = json.loads(args.export.read_text())
    cases = json.loads(args.cases.read_text())
    passing = {(c['expression'], c['expected'], c.get('operator', '==')): c['body'] for c in cases if c['up']}
    freshness = {mid: int(age) for mid, age in (item.split('=', 1) for item in args.mqtt_max_age)}
    config, report = migration.Converter(source, freshness).convert()
    if report['unmapped']:
        raise SystemExit('Benchmark preparation requires a fully mapped export')
    config['server'].update(bind='0.0.0.0:8787', database_path='/data/hora.db', auth_token='benchmark-only-viewer-token', rate_limit_burst=10000)
    config['channels'] = []
    config['updates'] = {'check': False}
    fixtures = {'http': {}, 'mqtt': {}, 'push': []}
    groups = {}
    for broker in config['brokers']:
        broker.update(url='mqtt://127.0.0.1:1883')
        broker.pop('username', None)
        broker.pop('password', None)
    if len(config['brokers']) > 1:
        raise SystemExit('This isolated harness supports one shared broker')
    for i, (row, original) in enumerate(zip(config['monitors'], source['monitors'])):
        row.update(name=f'Benchmark {i}', notify=[])
        row.pop('description', None)
        if 'group' in row:
            groups.setdefault(row['group'], f'Benchmark group {len(groups)}')
            row['group'] = groups[row['group']]
        row['check_cert'] = False
        payload = b'healthy'
        if 'jq' in row:
            key = (original['json_path'], str(original.get('expected_value', 'true')).lower(), original.get('json_path_operator') or '==')
            if key not in passing:
                raise SystemExit('A JSON assertion needs a passing fixture')
            payload = json.dumps(passing[key]).encode()
        elif 'keyword' in row:
            payload = ('' if row.get('keyword_invert') else row['keyword']).encode()
        encoded = base64.b64encode(payload).decode()
        if row['kind'] == 'http':
            row.update(target=f'http://127.0.0.1:18080/{i}', headers={})
            statuses = row.get('expected_statuses', [row.get('expected_status', 200)])
            status = next((s for s in statuses if 200 <= s <= 299), statuses[0])
            fixtures['http'][f'/{i}'] = {'body': encoded, 'status': status}
        elif row['kind'] == 'tcp': row['target'] = '127.0.0.1:18080'
        elif row['kind'] == 'icmp': row['target'] = '127.0.0.1'
        elif row['kind'] == 'dns':
            row.update(target='benchmark.test', dns_resolver='127.0.0.1:15353')
            row.pop('dns_expected', None)
        elif row['kind'] == 'mqtt':
            row['topic'] = f'benchmark/{i}'
            fixtures['mqtt'][row['topic']] = {'body': encoded, 'period': min(60, max(1, row['max_age_secs'] // 3)) if 'max_age_secs' in row else 0}
        elif row['kind'] == 'push':
            row['push_token'] = f'benchmark-push-token-{i}'
            fixtures['push'].append({'token': row['push_token'], 'period': min(60, max(1, row['interval_secs'] // 2))})
    for window in config['maintenance']: window['title'] = 'Benchmark maintenance'
    args.output_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    migration.private_write(args.output_dir / 'config.toml', migration.render(config))
    migration.private_write(args.output_dir / 'fixtures.json', json.dumps(fixtures))
    print(f"Prepared {len(config['monitors'])} monitors with loopback fixtures and no notification channels.")


if __name__ == '__main__':
    main()
