"""Sample the isolated daemon's Linux RSS and CPU; publish aggregate data only."""
import json
from pathlib import Path
import subprocess
import sys
import time

monitor, fixtures, output, duration = sys.argv[1:]
if int(duration) < 65: raise SystemExit('Measure for at least 65 seconds to include every initial probe')
rows = []
start = time.monotonic()
ticks = int(subprocess.check_output(['docker', 'exec', monitor, 'getconf', 'CLK_TCK']))
while time.monotonic() - start < int(duration):
    raw = subprocess.check_output(['docker', 'exec', monitor, 'sh', '-c', 'cat /proc/1/status; cat /proc/1/stat'], text=True)
    status = {}
    for line in raw.splitlines():
        if line.startswith(('VmRSS:', 'VmHWM:', 'Threads:')):
            key, value, *_ = line.split()
            status[key.rstrip(':')] = int(value)
    fields = raw.splitlines()[-1].rsplit(')', 1)[1].split()
    rows.append(dict(seconds=round(time.monotonic() - start, 2), rss_kib=status['VmRSS'], peak_kib=status['VmHWM'], threads=status['Threads'], cpu_seconds=(int(fields[11])+int(fields[12]))/ticks))
    time.sleep(2)
code = "import json,sqlite3; c=sqlite3.connect('/data/hora.db'); print(json.dumps({'checks':c.execute('select count(*) from checks').fetchone()[0], 'monitors_checked':c.execute('select count(distinct monitor_id) from checks').fetchone()[0], 'failed_checks':c.execute('select count(*) from checks where status=0').fetchone()[0]}))"
check = subprocess.run(['docker', 'exec', fixtures, 'python', '-c', code], capture_output=True, text=True)
counts = json.loads(check.stdout) if check.returncode == 0 else {'database_summary': 'unavailable'}
steady = [row for row in rows if row['seconds'] >= 60] or rows
summary = dict(duration_seconds=rows[-1]['seconds'], rss_final_mib=round(rows[-1]['rss_kib']/1024, 2), rss_peak_mib=round(max(r['peak_kib'] for r in rows)/1024, 2), steady_rss_min_mib=round(min(r['rss_kib'] for r in steady)/1024, 2), steady_rss_max_mib=round(max(r['rss_kib'] for r in steady)/1024, 2), cpu_percent_one_core=round(100*(rows[-1]['cpu_seconds']-rows[0]['cpu_seconds'])/(rows[-1]['seconds']-rows[0]['seconds']), 3), **counts)
summary['steady_cpu_percent_one_core'] = round(100 * (steady[-1]['cpu_seconds'] - steady[0]['cpu_seconds']) / (steady[-1]['seconds'] - steady[0]['seconds']), 3)
Path(output, 'metrics.json').write_text(json.dumps(dict(summary=summary, samples=rows), indent=2)+'\n')
print(json.dumps(summary, indent=2))
