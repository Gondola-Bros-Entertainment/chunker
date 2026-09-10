"""Black-box regression tests. Fake credentials and a loopback S3 server only.
Run: python3 tests/cli.py target/release/chunker
"""
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.parse

BINARY = str(Path(sys.argv.pop(1)).resolve())

class Store(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(('127.0.0.1', 0), Handler)
        self.objects = {}
        self.requests = []
        self.race_pointer = False
        self.corrupt_manifest = False
        self.race_manifest = False
        self.lock = threading.Lock()

def etag(body):
    return '"' + hashlib.md5(body).hexdigest() + '"'

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def key(self):
        return urllib.parse.unquote(urllib.parse.urlsplit(self.path).path).split('/audit/', 1)[-1]

    def reply(self, status, body=b''):
        self.send_response(status)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('ETag', etag(body))
        self.end_headers()
        if self.command != 'HEAD':
            self.wfile.write(body)

    def do_GET(self):
        with self.server.lock:
            key = self.key()
            self.server.requests.append(('GET', key))
            body = self.server.objects.get(key)
            self.reply(404 if body is None else 200, body or b'')

    def do_HEAD(self):
        with self.server.lock:
            key = self.key()
            self.server.requests.append(('HEAD', key))
            body = self.server.objects.get(key)
            if self.server.race_manifest and key == 'client/versions/v2/manifest.json':
                self.server.objects[key] = b'concurrent manifest'
                self.server.race_manifest = False
            self.reply(404 if body is None else 200, body or b'')

    def do_PUT(self):
        key = self.key()
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        if 'aws-chunked' in self.headers.get('Content-Encoding', ''):
            decoded = bytearray()
            while body:
                header, body = body.split(b'\r\n', 1)
                count = int(header.split(b';')[0], 16)
                if not count:
                    break
                decoded.extend(body[:count])
                body = body[count + 2:]
            body = bytes(decoded)
        with self.server.lock:
            self.server.requests.append(('PUT', key))
            current = self.server.objects.get(key)
            absent = self.headers.get('If-None-Match')
            matches = self.headers.get('If-Match')
            if (absent == '*' and current is not None) or (matches and (current is None or etag(current) != matches)):
                self.reply(412, b'<Error><Code>PreconditionFailed</Code></Error>')
                return
            if not absent and not matches:
                self.reply(400, b'<Error><Code>MissingCondition</Code></Error>')
                return
            self.server.objects[key] = body
            if key.endswith('/versions/v2/manifest.json'):
                if self.server.race_pointer:
                    self.server.objects['client/latest.txt'] = b'concurrent-v3\n'
                    self.server.race_pointer = False
                if self.server.corrupt_manifest:
                    self.server.objects[key] = b'{}'
            self.reply(200)

class CliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='chunker-tests-')
        self.root = Path(self.temp.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        self.source.joinpath('data.bin').write_bytes(b'abcdefghabcdefghXYZ')
        self.source.joinpath('empty').write_bytes(b'')
        self.output = self.root / 'chunked'
        self.store = Store()
        self.thread = threading.Thread(target=self.store.serve_forever, daemon=True)
        self.thread.start()
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(('R2_', 'AWS_'))}
        self.env.update(AWS_ACCESS_KEY_ID='fake-audit', AWS_SECRET_ACCESS_KEY='fake-audit',
                        AWS_ENDPOINT_URL=f'http://127.0.0.1:{self.store.server_port}', AWS_REGION='us-east-1',
                        NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost')
        self.chunk()

    def tearDown(self):
        self.store.shutdown()
        self.store.server_close()
        self.thread.join()
        self.temp.cleanup()

    def run_cli(self, args, success=True):
        result = subprocess.run([BINARY, *map(str, args)], env=self.env, text=True, capture_output=True, timeout=20)
        if success:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        return result

    def chunk(self, source=None, output=None, version='v1', chunk_size=8, level=12, success=True):
        return self.run_cli(['chunk', '--input', source or self.source, '--output', output or self.output,
                             '--version', version, '--game', 'audit', '--platform', 'linux', '--chunk-size', chunk_size,
                             '--zstd-level', level], success)

    def publish(self, version='v1', output=None, success=True, extra=()):
        return self.run_cli(['publish', '--chunks', output or self.output, '--version', version,
                             '--bucket', 'audit', '--prefix', 'client', *extra], success)

    def patch(self, extra=(), success=True):
        replacement = self.root / 'replacement'
        replacement.write_bytes(b'abcdefgh')
        return self.run_cli(['patch', '--bucket', 'audit', '--prefix', 'client', '--base-version', 'v1',
                             '--version', 'v2', '--chunk-size', '8', '--override', f'data.bin={replacement}', *extra], success)

    def pointer(self):
        return self.store.objects.get('client/latest.txt')

    def test_chunk_hashes_dedup_and_empty_files(self):
        value = json.loads(self.output.joinpath('manifest.json').read_text())
        self.assertEqual(value['totalSize'], 19)
        self.assertEqual(len(value['chunks']), 2)
        self.assertEqual(value['files']['data.bin']['chunks'], [hashlib.sha256(b'abcdefgh').hexdigest()] * 2 + [hashlib.sha256(b'XYZ').hexdigest()])
        self.assertEqual(value['files']['empty'], {'size': 0, 'chunks': []})
        self.publish()
        self.assertEqual(self.pointer(), b'v1\n')

    def test_missing_input(self):
        self.chunk(source=self.root / 'missing', success=False)
        self.assertFalse(self.store.requests)

    def test_zero_and_oversized_chunks(self):
        for size in [0, 67108865, 18446744073709551615]:
            self.chunk(chunk_size=size, success=False)
        self.assertFalse(self.store.requests)

    def test_output_inside_input(self):
        self.chunk(output=self.source / 'output', success=False)

    def test_case_collision(self):
        # Directory spelling collision works even on case-insensitive developer filesystems.
        manifest = json.loads(self.output.joinpath('manifest.json').read_text())
        manifest['files']['A/x'] = {'size': 0, 'chunks': []}
        manifest['files']['a/y'] = {'size': 0, 'chunks': []}
        self.output.joinpath('manifest.json').write_text(json.dumps(manifest))
        self.publish(success=False)
        self.assertFalse(self.store.requests)

    def test_version_mismatch(self):
        self.publish(version='v2', success=False)
        self.assertFalse(self.store.requests)

    def test_missing_or_corrupt_local_chunks(self):
        chunk = next(self.output.joinpath('chunks').iterdir())
        saved = chunk.read_bytes()
        chunk.write_bytes(bytes(len(saved)))
        self.publish(success=False)
        chunk.unlink()
        self.publish(success=False)
        self.assertFalse(self.store.requests)

    def test_zero_concurrency(self):
        self.publish(success=False, extra=['--concurrency', '0'])
        self.assertFalse(self.store.requests)

    def test_patch_preflight_paths_conflicts_and_force(self):
        replacement = self.root / 'other'
        replacement.write_bytes(b'x')
        for args in [['--override', f'../escape={replacement}'], ['--remove', 'data.bin'],
                     ['--remove', '../escape'], ['--force']]:
            self.patch(args, success=False)
        self.assertFalse(self.store.requests)

    def test_existing_version_is_immutable(self):
        self.publish()
        original = dict(self.store.objects)
        self.publish(success=False)
        self.assertEqual(self.store.objects, original)

    def test_patch_equals_full_tree(self):
        self.publish()
        replacement = self.root / 'replacement'
        replacement.write_bytes(b'abcdefghABCDEFGHABC')
        self.run_cli(['patch', '--bucket', 'audit', '--prefix', 'client', '--version', 'v2', '--chunk-size', '8',
                      '--override', f'data.bin={replacement}', '--remove', 'empty'])
        self.source.joinpath('data.bin').write_bytes(replacement.read_bytes())
        self.source.joinpath('empty').unlink()
        full = self.root / 'full'
        self.chunk(output=full, version='v2')
        actual = json.loads(self.store.objects['client/versions/v2/manifest.json'])
        expected = json.loads(full.joinpath('manifest.json').read_text())
        actual.pop('generatedAt'); expected.pop('generatedAt')
        self.assertEqual(actual, expected)
        self.assertEqual(self.pointer(), b'v2\n')

    def test_missing_inherited_chunk_preserves_pointer(self):
        self.publish()
        inherited = hashlib.sha256(b'abcdefgh').hexdigest()
        del self.store.objects[f'client/chunks/{inherited}.zst']
        self.patch(success=False)
        self.assertEqual(self.pointer(), b'v1\n')

    def test_pointer_race_preserves_other_writer(self):
        self.publish()
        self.store.race_pointer = True
        self.patch(success=False)
        self.assertEqual(self.pointer(), b'concurrent-v3\n')

    def test_manifest_race_preserves_other_writer(self):
        self.publish()
        self.store.race_manifest = True
        self.patch(success=False)
        self.assertEqual(self.store.objects['client/versions/v2/manifest.json'], b'concurrent manifest')
        self.assertEqual(self.pointer(), b'v1\n')

    def test_failed_manifest_readback_preserves_pointer(self):
        self.publish()
        self.store.corrupt_manifest = True
        self.patch(success=False)
        self.assertEqual(self.pointer(), b'v1\n')

    def test_existing_chunk_encoding_is_preserved(self):
        self.source.joinpath('data.bin').write_text(''.join(f'packet={i % 73} body={"value" * (i % 9)}\n' for i in range(1500)))
        first = self.root / 'first'
        second = self.root / 'second'
        self.chunk(output=first, chunk_size=65536, level=1)
        self.chunk(output=second, chunk_size=65536, level=19, version='v2')
        before = {p.name: p.read_bytes() for p in first.joinpath('chunks').iterdir()}
        other = {p.name: p.read_bytes() for p in second.joinpath('chunks').iterdir()}
        self.assertNotEqual(before, other)
        self.publish(output=first)
        self.publish(version='v2', output=second)
        published = json.loads(self.store.objects['client/versions/v2/manifest.json'])
        for hash_, chunk in published['chunks'].items():
            canonical = before[hash_ + '.zst']
            self.assertEqual(self.store.objects[f'client/chunks/{hash_}.zst'], canonical)
            self.assertEqual(chunk['compressedSize'], len(canonical))

if __name__ == '__main__':
    unittest.main(verbosity=2)
