import base64
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote_plus

count = 0
lock = threading.Lock()

class Handler(BaseHTTPRequestHandler):
    def respond(self, status, obj):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.respond(200, {'count': count})

    def do_POST(self):
        global count
        length = self.headers.get('Content-Length')
        if self.path != '/token' or length is None or self.headers.get('Transfer-Encoding'):
            self.respond(400, {'error': 'invalid_request'})
            return
        try:
            form = parse_qs(self.rfile.read(int(length)).decode(), keep_blank_values=True)
            auth = self.headers.get('Authorization', '')
            if auth.startswith('Basic '):
                raw = base64.b64decode(auth[6:], validate=True).decode()
                client, secret = (unquote_plus(x) for x in raw.split(':', 1))
            else:
                client = form.get('client_id', [''])[0]
                secret = form.get('client_secret', [''])[0]
            valid = form.get('grant_type') == ['client_credentials'] and client == os.environ['OAUTH_CLIENT_ID'] and secret == os.environ['OAUTH_SERVER_SECRET']
        except (ValueError, UnicodeError):
            valid = False
        with lock:
            count += 1
            token_number = count
        if not valid:
            self.respond(401, {'error': 'invalid_client'})
            return
        self.respond(200, {'access_token': f'tok-{token_number}', 'token_type': 'Bearer', 'expires_in': int(os.environ.get('OAUTH_TTL', '2'))})

    def log_message(self, *args):
        pass

ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Handler).serve_forever()
