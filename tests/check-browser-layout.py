#!/usr/bin/env python3
"""Read-only UI geometry check via an agent-browser managed Chrome CDP session.

Open the actual CPR plugin through the redacted loopback browser proxy first.
No model runs, credential edits, screenshots, or account data are collected.
--candidate applies local CSS/dialog logic only to this diagnostic browser DOM.
Requires websocket-client; usage: python3 tests/check-browser-layout.py --port PORT [--candidate]
"""
import argparse
import json
from pathlib import Path
import re
import time
import urllib.request

import websocket

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--port', type=int, required=True)
parser.add_argument('--candidate', action='store_true')
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
version = json.load(urllib.request.urlopen(f'http://127.0.0.1:{args.port}/json/version', timeout=5))
ws = websocket.create_connection(version['webSocketDebuggerUrl'], timeout=20, suppress_origin=True)
serial = 0


def call(method, params=None, session=None):
    global serial
    serial += 1
    request = {'id': serial, 'method': method, 'params': params or {}}
    if session:
        request['sessionId'] = session
    ws.send(json.dumps(request))
    while True:
        answer = json.loads(ws.recv())
        if answer.get('id') == serial:
            if 'error' in answer:
                raise RuntimeError('cdp_command_failed')
            return answer['result']


def evaluate(session, expression):
    result = call('Runtime.evaluate', {'expression': expression, 'awaitPromise': True,
                                      'returnByValue': True}, session)
    if 'exceptionDetails' in result:
        raise RuntimeError('browser_evaluation_failed')
    return result.get('result', {}).get('value')


def attach(target):
    return call('Target.attachToTarget', {'targetId': target['targetId'], 'flatten': True})['sessionId']


def metrics(session):
    return evaluate(session, """(async()=>{
      const d=document.querySelector('dialog[open]');
      const p=document.querySelector('.dialog-backdrop');
      let v=null;
      if(p) v=await new Promise(resolve=>{
        const o=new IntersectionObserver(([e])=>{o.disconnect();resolve(e.intersectionRect);});
        o.observe(p);
      });
      const r=d?.getBoundingClientRect();
      return {body:Math.ceil(document.body.getBoundingClientRect().height), viewport:innerHeight,
        open:!!d, top:r?.top, bottom:r?.bottom, left:r?.left, right:r?.right,
        visibleTop:v?.top,visibleBottom:v?.bottom,visibleLeft:v?.left,visibleRight:v?.right,
        backdrops:document.querySelectorAll('.dialog-backdrop').length};
    })()""")


try:
    targets = call('Target.getTargets')['targetInfos']
    parents = [t for t in targets if t['type'] == 'page' and
               t.get('url', '').startswith('http://127.0.0.1:18761/')]
    assert len(parents) == 1, 'one_diagnostic_proxy_tab_required'
    parent = attach(parents[0])
    evaluate(parent, """(()=>{const frames=document.querySelectorAll('iframe');
      if(frames.length!==1)throw Error('one_plugin_iframe_required');
      return true;})()""")
    candidates = []
    for target in targets:
        if target['type'] != 'iframe':
            continue
        session = attach(target)
        if evaluate(session, '!!window.codexProxyPlugin && !!document.getElementById("mt-rows")'):
            candidates.append(session)
    assert len(candidates) == 1, 'one_diagnostic_plugin_required'
    frame = candidates[0]
    evaluate(frame, "document.querySelectorAll('dialog[open]').forEach(d=>d.close());true")
    if args.candidate:
        css = (root / 'web/style.css').read_text()
        rules = [re.search(pattern, css).group() for pattern in (
            r':root\[data-cpr-plugin-host\] main \{[^}]+\}',
            r'dialog\[open\] \{[^}]+\}', r'\.result-dialog \{[^}]+\}')]
        js = (root / 'web/components.js').read_text()
        function = re.search(r'function showDialog\(dialog\) \{.*?\n\}', js, re.S).group()
        evaluate(frame, """(()=>{
          document.querySelector('main').style.removeProperty('min-height');
          document.querySelectorAll('dialog').forEach(d=>d.removeAttribute('style'));
          const s=document.createElement('style');s.textContent=%s;document.head.append(s);
          showDialog=(%s);return true;})()""" % (json.dumps('\n'.join(rules)), function))
    time.sleep(0.5)
    heights = []
    for _ in range(8):
        heights.append(metrics(frame)['body'])
        time.sleep(0.15)
    assert max(heights) - min(heights) <= 1, 'iframe_height_feedback_loop'
    reports = []
    for suite, prefix in [('modeltrace', 'mt'), ('pelican', 'pl')]:
        evaluate(frame, "document.getElementById('tab-%s').click();true" % suite)
        time.sleep(0.4)
        evaluate(frame, "document.querySelector('#%s-rows .row:not(.open) [data-row]')?.click();true" % prefix)
        time.sleep(0.25)
        count = evaluate(frame, "document.querySelectorAll('[data-%s-detail]').length" % prefix)
        assert count > 0, 'existing_detail_record_required'
        for repeat in range(3):
            evaluate(frame, "document.querySelector('[data-%s-detail]').click();true" % prefix)
            time.sleep(0.5)
            samples = []
            for _ in range(4):
                sample = metrics(frame)
                assert sample['open'] and sample['backdrops'] == 1, 'one_open_dialog_required'
                assert sample['top'] >= sample['visibleTop'] - 2, 'dialog_above_visible_viewport'
                assert sample['bottom'] <= sample['visibleBottom'] + 2, 'dialog_below_visible_viewport'
                assert sample['left'] >= sample['visibleLeft'] - 2, 'dialog_left_of_visible_viewport'
                assert sample['right'] <= sample['visibleRight'] + 2, 'dialog_right_of_visible_viewport'
                samples.append(sample)
                time.sleep(0.12)
            assert max(s['body'] for s in samples) - min(s['body'] for s in samples) <= 1, 'open_dialog_height_drift'
            evaluate(frame, "document.querySelector('dialog[open]').close();true")
            time.sleep(0.2)
            closed = metrics(frame)
            assert not closed['open'] and closed['backdrops'] == 0, 'dialog_cleanup_incomplete'
            reports.append({'suite': suite, 'repeat': repeat + 1, 'geometry': samples[-1],
                            'closed_without_backdrop': True})
    # Test the real host account-page navigation too, without reading any row contents.
    evaluate(parent, """(async()=>{
      await document.querySelector('#app').__vue_app__.config.globalProperties.$router.push('/accounts');
      return true;})()""")
    time.sleep(1)
    accounts = evaluate(parent, """(()=>({
      route:document.querySelector('#app').__vue_app__.config.globalProperties.$router.currentRoute.value.path==='/accounts',
      table:!!document.querySelector('table'),iframes:document.querySelectorAll('iframe').length,
      versionCurrent:document.body.innerText.includes('3.26.2')
    }))()""")
    assert accounts['route'] and accounts['table'] and not accounts['iframes'], 'account_page_navigation_failed'
    print(json.dumps({'candidate': args.candidate, 'height_samples': heights,
                      'detail_checks': reports, 'account_page': accounts}, ensure_ascii=False))
finally:
    ws.close()
