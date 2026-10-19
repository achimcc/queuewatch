{ pkgs, module, package }:
let
  # One fake Radarr and one fake OIDC provider, on the same machine as both
  # halves. The provider only has to survive discovery: the test never signs
  # anyone in, it checks that nobody gets past the door without it.
  fake = pkgs.writeText "fake.py" ''
    import json, threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    MOVIE = {"title": "Heat", "year": 1995, "tmdbId": 949}
    # The second record has no movie: Radarr then shows only the release
    # title, and for a grab by URL that is the URL, with the indexer's key.
    QUEUE = {"page": 1, "pageSize": 1000, "totalRecords": 2, "records": [{
        "movieId": 1, "movie": MOVIE, "title": "Heat.1995.1080p", "size": 1000, "sizeleft": 250,
        "status": "downloading", "trackedDownloadStatus": "ok", "trackedDownloadState": "downloading",
        "errorMessage": "https://tracker.example/announce?passkey=VMSEKRET",
        "statusMessages": [{"title": "x", "messages": ["/data/torrents/VMPATH"]}],
        "downloadId": "SABnzbd_nzo_1", "protocol": "usenet", "estimatedCompletionTime": "2099-01-01T00:00:00Z"},
      {
        "movieId": 0, "title": "https://idx.example/getnzb?apikey=VMIDXKEY", "size": 1000, "sizeleft": 1000,
        "status": "queued", "trackedDownloadStatus": "ok", "trackedDownloadState": "downloading",
        "downloadId": "SABnzbd_nzo_2", "protocol": "usenet"}]}

    class Radarr(BaseHTTPRequestHandler):
        def _json(self, obj):
            body = json.dumps(obj).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
        def do_GET(self):
            if self.headers.get("X-Api-Key") != "vm-radarr-key":
                self.send_response(401); self.end_headers(); return
            self._json(QUEUE if self.path.startswith("/api/v3/queue") else [])
        def log_message(self, *a): pass

    ISSUER = "http://127.0.0.1:19000"
    class Oidc(BaseHTTPRequestHandler):
        def _json(self, obj):
            body = json.dumps(obj).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
        def do_GET(self):
            if self.path == "/.well-known/openid-configuration":
                self._json({"issuer": ISSUER, "authorization_endpoint": ISSUER + "/authorize",
                            "token_endpoint": ISSUER + "/token", "jwks_uri": ISSUER + "/jwks",
                            "response_types_supported": ["code"], "subject_types_supported": ["public"],
                            "id_token_signing_alg_values_supported": ["RS256"]})
            elif self.path == "/jwks":
                self._json({"keys": []})
            else:
                self.send_response(404); self.end_headers()
        def log_message(self, *a): pass

    threading.Thread(target=HTTPServer(("127.0.0.1", 17878), Radarr).serve_forever, daemon=True).start()
    HTTPServer(("127.0.0.1", 19000), Oidc).serve_forever()
  '';
  credentialsDirectory = "/run/credentials/queuewatch-collect.service";
in
pkgs.testers.runNixOSTest {
  name = "queuewatch";

  nodes.machine =
    { ... }:
    {
      imports = [ module ];

      systemd.services.fake = {
        wantedBy = [ "multi-user.target" ];
        before = [ "queuewatch-collect.service" "queuewatch-serve.service" ];
        serviceConfig.ExecStart = "${pkgs.python3}/bin/python3 ${fake}";
      };

      environment.etc."qw/radarr".text = "vm-radarr-key\n";
      environment.etc."qw/token".text = "vm-snapshot-token\n";
      # 128 characters: read as base64 that is 96 bytes. 64 hex digits would
      # decode to only 48 and be refused, which is the check doing its job.
      environment.etc."qw/cookie".text = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
      environment.etc."qw/oidc".text = "vm-client-secret\n";

      services.queuewatch = {
        inherit package;
        collect = {
          enable = true;
          snapshotTokenFile = "/etc/qw/token";
          credentials.radarr = "/etc/qw/radarr";
          # Everything here is on localhost, which the list always allows;
          # the entry only switches the restriction on.
          allowedClients = [ "192.0.2.1" ];
          settings = {
            listen = "127.0.0.1:8095";
            interval_secs = 2;
            radarr = [
              {
                label = "Movies";
                url = "http://127.0.0.1:17878";
                api_key_file = "${credentialsDirectory}/radarr";
              }
            ];
          };
        };
        serve = {
          enable = true;
          snapshotTokenFile = "/etc/qw/token";
          cookieKeyFile = "/etc/qw/cookie";
          oidcClientSecretFile = "/etc/qw/oidc";
          settings = {
            listen = "127.0.0.1:8096";
            public_url = "http://127.0.0.1:8096";
            snapshot_url = "http://127.0.0.1:8095/snapshot";
            oidc = {
              issuer = "http://127.0.0.1:19000";
              client_id = "queuewatch";
            };
          };
        };
      };
    };

  testScript = ''
    machine.wait_for_unit("fake.service")
    machine.wait_for_unit("queuewatch-collect.service")
    machine.wait_for_unit("queuewatch-serve.service")
    machine.wait_for_open_port(8095)
    machine.wait_for_open_port(8096)

    def status(args):
        return machine.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' {args}").strip()

    with subtest("the snapshot needs the right token"):
        assert status("http://127.0.0.1:8095/snapshot") == "401"
        assert status("-H 'Authorization: Bearer wrong-token' http://127.0.0.1:8095/snapshot") == "401"
        machine.wait_until_succeeds(
            "curl -fsS -H 'Authorization: Bearer vm-snapshot-token' http://127.0.0.1:8095/snapshot | grep -q Heat"
        )

    with subtest("nothing from a free-text field or a URL-shaped name crosses into the snapshot"):
        snapshot = machine.succeed("curl -fsS -H 'Authorization: Bearer vm-snapshot-token' http://127.0.0.1:8095/snapshot")
        assert "VMSEKRET" not in snapshot, "a passkey from errorMessage reached the snapshot"
        assert "VMPATH" not in snapshot, "a path from statusMessages reached the snapshot"
        assert "VMIDXKEY" not in snapshot, "an indexer key from a release title reached the snapshot"
        # Withheld, not lost: the entry is there under the neutral marker.
        assert "(unnamed download)" in snapshot, snapshot

    with subtest("the page's configuration holds no service credential"):
        env = machine.succeed("systemctl show -p Environment --value queuewatch-serve.service").split()
        paths = [e.split("=", 1)[1] for e in env if e.startswith("QUEUEWATCH_CONFIG=")]
        assert len(paths) == 1, env
        config = machine.succeed(f"cat {paths[0]}")
        assert "snapshot_url" in config, config
        for key in ["api_key_file", "password_file", "radarr"]:
            assert key not in config, f"{key} in the page's configuration"
        unit = machine.succeed("systemctl cat queuewatch-serve.service")
        names = sorted(
            line.split("=", 1)[1].split(":", 1)[0]
            for line in unit.splitlines()
            if line.startswith("LoadCredential=")
        )
        assert names == ["cookie-key", "oidc-client-secret", "snapshot-token"], names

    with subtest("the collector's keys are credentials, and its unit is fenced"):
        machine.succeed("systemctl cat queuewatch-collect.service | grep -q 'LoadCredential=radarr:'")
        deny = machine.succeed("systemctl show -p IPAddressDeny --value queuewatch-collect.service")
        assert deny.strip() != "", "allowedClients did not switch the IP restriction on"
        for unit in ["queuewatch-collect", "queuewatch-serve"]:
            limit = machine.succeed(f"systemctl show -p MemoryMax --value {unit}.service").strip()
            assert limit == str(256 * 1024 * 1024), f"{unit}: MemoryMax={limit}"

    with subtest("the page lets nobody in without signing in"):
        machine.succeed("curl -fsS http://127.0.0.1:8096/healthz")
        assert status("http://127.0.0.1:8096/style.css") == "200"
        location = machine.succeed("curl -s -o /dev/null -w '%{redirect_url}' http://127.0.0.1:8096/")
        assert "/auth/login" in location, location
        authorize = machine.succeed("curl -s -o /dev/null -w '%{redirect_url}' http://127.0.0.1:8096/auth/login")
        assert authorize.startswith("http://127.0.0.1:19000/authorize"), authorize
        assert "code_challenge=" in authorize, authorize
        # A callback nobody started: no pending-login cookie to compare with.
        assert status("'http://127.0.0.1:8096/auth/callback?code=x&state=y'") == "400"
        # A sign-out forged from another site.
        assert status("-X POST -H 'Sec-Fetch-Site: cross-site' http://127.0.0.1:8096/auth/logout") == "403"
  '';
}
