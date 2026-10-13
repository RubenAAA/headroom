"""Re-forward captured client bodies through today's proxy, offline.

Usage: reforward.py OUT_DIR CAPTURE_DIR... [--sessions N]

A capture's out/ bodies show what the proxy forwarded the day it was taken.
To judge today's settings, this starts the installed headroom-proxy with
the flags in ~/.headroom-flags.sh against a fake upstream on loopback, posts
every main-model client body in capture order, and lets the proxy's own
capture (HEADROOM_CAPTURE_DIR) write OUT_DIR/req-*.json and
OUT_DIR/out/<id>.json -- the layout select.py reads.

Nothing leaves the machine and no live state is touched: the run happens
in a network namespace with only loopback (`unshare -rn`), and HOME points
into OUT_DIR, so the ledger, gate files, stores and memory start empty. Because the stores start
empty, recall injection has nothing to inject; a live turn could differ by
that block.

--sessions N keeps the N sessions with the most turns (default: all).
--set --flag=value overrides one flag from the flags file, for an A/B
against a run without it (repeatable).
"""
import collections, glob, json, os, shutil, socket, subprocess, sys, threading, time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MAIN_MODELS = ('claude-opus', 'claude-sonnet', 'claude-fable')
BETA = 'oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27'


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class Upstream(BaseHTTPRequestHandler):
    """Answers every call with a short text reply, streamed when asked."""

    def log_message(self, format, *args):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('content-length') or 0))
        try:
            req = json.loads(body)
        except ValueError:
            req = {}
        if 'count_tokens' in self.path:
            return self.send_json({'input_tokens': len(body) // 4})
        usage = {'input_tokens': 10, 'output_tokens': 5,
                 'cache_read_input_tokens': len(body) // 4, 'cache_creation_input_tokens': 0}
        msg = {'id': 'msg_offline', 'type': 'message', 'role': 'assistant',
               'model': req.get('model', ''), 'content': [{'type': 'text', 'text': 'ok'}],
               'stop_reason': 'end_turn', 'stop_sequence': None, 'usage': usage}
        if not req.get('stream'):
            return self.send_json(msg)
        events = [
            ('message_start', {'type': 'message_start', 'message': dict(msg, content=[], stop_reason=None)}),
            ('content_block_start', {'type': 'content_block_start', 'index': 0,
                                     'content_block': {'type': 'text', 'text': ''}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'text_delta', 'text': 'ok'}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
            ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn'},
                               'usage': {'output_tokens': 5}}),
            ('message_stop', {'type': 'message_stop'}),
        ]
        data = ''.join(f'event: {e}\ndata: {json.dumps(d)}\n\n' for e, d in events).encode()
        self.send_response(200)
        self.send_header('content-type', 'text/event-stream')
        self.send_header('content-length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def send_json(self, obj):
        data = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def proxy_flags(home):
    """The live flag array and the HEADROOM_* variables the flags file
    exports, with $HOME read as the scratch home."""
    out = subprocess.run(
        ['bash', '-c', 'source "$1"; printf "%s\\0" "${HEADROOM_FLAGS[@]}"; '
         'printf "\\1\\0"; env -0', '_', os.path.expanduser('~/.headroom-flags.sh')],
        capture_output=True, env={'HOME': home, 'PATH': os.environ['PATH']}, check=True).stdout
    flags, _, env = out.partition(b'\1\0')
    exported = dict(kv.decode().split('=', 1) for kv in env.split(b'\0')
                    if kv.startswith(b'HEADROOM_') and b'=' in kv)
    return [a.decode() for a in flags.split(b'\0') if a], exported


def override(flags, name, value):
    """Set `name` to `value` whether the file wrote `--x v` or `--x=v`."""
    for i, a in enumerate(flags):
        if a == name and i + 1 < len(flags):
            flags[i + 1] = value
            return
        if a.startswith(name + '='):
            flags[i] = f'{name}={value}'
            return
    flags += [name, value]


def load_body(path):
    b = json.load(open(path)).get('body')
    return json.loads(b) if isinstance(b, str) else b


def records(dirs, sessions):
    """(ts, seq, session, path) per main-model body. Paths, not bodies: the
    four captures hold 14,680 bodies, and holding them parsed took 13.8 GB
    and set earlyoom on the live proxy."""
    recs = []
    for d in dirs:
        for path in glob.glob(os.path.join(d, 'req-*.json')):
            try:
                r = json.load(open(path))
            except ValueError:
                continue
            b = r.get('body')
            b = json.loads(b) if isinstance(b, str) else b
            if (r.get('endpoint') == 'anthropic' and isinstance(b, dict)
                    and str(b.get('model', '')).startswith(MAIN_MODELS)):
                recs.append((r.get('ts_ms', 0), r.get('seq', 0), r.get('session_key'), path))
    if sessions:
        keep = {k for k, _ in collections.Counter(r[2] for r in recs).most_common(sessions)}
        recs = [r for r in recs if r[2] in keep]
    return sorted(recs, key=lambda r: (r[0], r[1]))


def main():
    if not os.environ.get('REFORWARD_NETNS'):
        # Loopback only: the proxy cannot reach a provider whatever it tries.
        os.execvpe('unshare', ['unshare', '-rn', 'sh', '-c',
                               'ip link set lo up && exec "$@"', '_',
                               sys.executable, *sys.argv],
                   dict(os.environ, REFORWARD_NETNS='1'))
    args = sys.argv[1:]
    sessions = None
    if '--sessions' in args:
        i = args.index('--sessions')
        sessions = int(args[i + 1])
        del args[i:i + 2]
    sets = []
    while '--set' in args:
        i = args.index('--set')
        sets.append(args[i + 1].split('=', 1))
        del args[i:i + 2]
    out_dir, dirs = os.path.abspath(args[0]), args[1:]
    if os.path.exists(os.path.join(out_dir, 'out')):
        sys.exit(f'{out_dir} already holds a capture; pick a new directory')
    home = os.path.join(out_dir, '.home')
    os.makedirs(home)
    recs = records(dirs, sessions)
    print(f'{len(recs)} bodies from {len({r[2] for r in recs})} sessions', flush=True)

    up_port, px_port = free_port(), free_port()
    upstream = ThreadingHTTPServer(('127.0.0.1', up_port), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    flags, exported = proxy_flags(home)
    for name, value in sets:
        override(flags, name, value)
    env = {**os.environ, **exported, 'HOME': home,
           'XDG_STATE_HOME': os.path.join(home, '.local/state'),
           'HF_HOME': os.environ.get('HF_HOME', os.path.expanduser('~/.cache/huggingface')),
           'HEADROOM_CAPTURE_DIR': out_dir, 'HEADROOM_MODEL_ROUTER_ENABLED': '0'}
    for k in [k for k in env if k.startswith(('ANTHROPIC_', 'OPENCODE_', 'HEADROOM_ZEN_'))]:
        del env[k]
    log = open(os.path.join(out_dir, 'proxy.log'), 'w')
    proxy = subprocess.Popen(
        [shutil.which('headroom-proxy') or os.path.expanduser('~/.local/bin/headroom-proxy'),
         '--listen', f'127.0.0.1:{px_port}', '--upstream', f'http://127.0.0.1:{up_port}',
         '--ctx-capture=true', '--ctx-offload=true', '--ctx-inject=true', *flags],
        env=env, stdout=log, stderr=subprocess.STDOUT)
    base = f'http://127.0.0.1:{px_port}'
    try:
        for _ in range(120):
            try:
                urllib.request.urlopen(base + '/healthz', timeout=2)
                break
            except OSError:
                if proxy.poll() is not None:
                    sys.exit(f'proxy exited; see {out_dir}/proxy.log')
                time.sleep(0.5)
        else:
            sys.exit('proxy did not come up')
        failed = 0
        for i, (_, _, _, path) in enumerate(recs):
            body = load_body(path)
            headers = {'content-type': 'application/json', 'anthropic-version': '2023-06-01',
                       'anthropic-beta': BETA, 'authorization': 'Bearer sk-ant-oat01-offline',
                       'user-agent': 'claude-cli/offline-reforward'}
            try:
                sid = json.loads((body.get('metadata') or {}).get('user_id') or '{}').get('session_id')
            except (ValueError, AttributeError):
                sid = None
            if sid:
                headers['x-claude-code-session-id'] = sid
            req = urllib.request.Request(base + '/v1/messages', data=json.dumps(body).encode(),
                                         headers=headers)
            try:
                urllib.request.urlopen(req, timeout=300).read()
            except OSError as e:
                failed += 1
                print(f'body {i}: {e}', flush=True)
            if i % 100 == 99:
                print(f'{i + 1}/{len(recs)}', flush=True)
        time.sleep(2)  # capture writes land on a background thread
    finally:
        proxy.terminate()
        proxy.wait(timeout=30)
        upstream.shutdown()
    outs = len(os.listdir(os.path.join(out_dir, 'out'))) if os.path.isdir(os.path.join(out_dir, 'out')) else 0
    print(f'done: {len(recs) - failed} forwarded, {failed} failed, {outs} forwarded bodies captured')


if __name__ == '__main__':
    main()
