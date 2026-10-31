"""Synthetic assertion patterns; deliberately contains no installation data."""
import json
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from jsonata_to_jq import translate


def cases():
    rows = []
    def add(expression, expected, inputs):
        for body, up in inputs:
            rows.append(dict(expression=expression, query=translate(expression), expected=str(expected).lower(), body=body, up=up))
    add('$ > 100', 'true', [(101, True), (100, False), (None, False), ({}, False)])
    add('$ >= 0 and $ <= 7', 'true', [(0, True), (7, True), (-1, False), (8, False), (None, False)])
    add('$ >= 0 and $ <= 14', 'true', [(14, True), (15, False), ({}, False)])
    add('$count($[source in ["a", "b"]])', 0, [([], True), ([{'source': 'c'}], True), ([{'source': 'a'}], False), ([{'source': 'b'}, {'source': 'c'}], False), ({}, True)])
    for precision in (0, 1):
        expr = f'$round($number(data.result[0].value[1]), {precision})'
        expected = 2 if precision == 0 else 2.4
        values = [('2.4', True), ('2.5', precision == 0), ('2.35', True), ('2.45', True), ('3.5', False), ('bad', False)]
        add(expr, expected, [({'data': {'result': [{'value': [0, value]}]}}, up) for value, up in values] + [({}, False), ({'data': {'result': []}}, False)])
    path = 'results[0].series[0].values'
    def influx(values):
        return {'results': [{'series': [{'values': [[0, x] for x in values]}]}]}
    add(f'{path}[0][1] > 0 ? 1 : 0', 1, [(influx([1]), True), (influx([0]), False), (influx([-1]), False), ({}, False), ({'results': [{}]}, False), (influx([None]), False)])
    regression = f'($a := {path}[0][1]; $b := {path}[1][1]; $c := {path}[2][1]; $exists($a) and $exists($b) and $exists($c) and ($a < $b or $b < $c) ? 1 : 0)'
    add(regression, 0, [(influx([3, 2, 1]), True), (influx([2, 3, 1]), False), (influx([3, 1, 2]), False), ({}, True), (influx([]), True), (influx([3, 2]), True), (influx([None, 2, 1]), False)])
    return rows


if __name__ == '__main__':
    print(json.dumps(cases()))
