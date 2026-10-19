# services.queuewatch — the collector (holds the API keys) and the page
# (holds none). They are separate units with separate users, and on a
# well-split deployment they run on separate machines: enable only the half
# that belongs there.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.queuewatch;
  settingsFormat = pkgs.formats.toml { };
  collectCredentials = "/run/credentials/queuewatch-collect.service";
  serveCredentials = "/run/credentials/queuewatch-serve.service";

  collectFile = settingsFormat.generate "queuewatch-collect.toml" {
    collect = cfg.collect.settings // {
      token_file = "${collectCredentials}/snapshot-token";
    };
  };
  serveFile = settingsFormat.generate "queuewatch-serve.toml" {
    serve = cfg.serve.settings // {
      snapshot_token_file = "${serveCredentials}/snapshot-token";
      cookie_key_file = "${serveCredentials}/cookie-key";
      oidc = (cfg.serve.settings.oidc or { }) // {
        client_secret_file = "${serveCredentials}/oidc-client-secret";
      };
    };
  };

  hardening = user: {
    User = user;
    Group = user;
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    PrivateDevices = true;
    NoNewPrivileges = true;
    RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
    RestrictNamespaces = true;
    RestrictRealtime = true;
    LockPersonality = true;
    MemoryDenyWriteExecute = true;
    SystemCallFilter = [ "@system-service" "~@privileged" ];
    SystemCallArchitectures = "native";
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectKernelLogs = true;
    ProtectControlGroups = true;
    ProtectClock = true;
    ProtectHostname = true;
    ProtectProc = "invisible";
    ProcSubset = "pid";
    RestrictSUIDSGID = true;
    RemoveIPC = true;
    CapabilityBoundingSet = "";
    UMask = "0077";
    # A bound, not a fit: both halves hold a few megabytes. The point is that
    # a service answering without end (bodies are capped at 32 MiB each, but
    # sources run side by side) ends this unit, not the machine's memory.
    MemoryMax = "256M";
    TasksMax = 64;
    # Nothing to keep: the collector holds its snapshot in memory, the page
    # its sessions in a cookie.
    Restart = "on-failure";
    RestartSec = "5s";
  };

  credentialType = lib.types.str;
  credentialDescription = ''
    In either form `LoadCredential` accepts: an absolute path to a file, or
    the bare name of a credential the service manager itself received (from
    a container manager, say).
  '';
in
{
  options.services.queuewatch = {
    package = lib.mkOption {
      type = lib.types.package;
      description = "The queuewatch package to run.";
    };

    collect = {
      enable = lib.mkEnableOption "the queuewatch collector, which holds the API keys";

      settings = lib.mkOption {
        type = settingsFormat.type;
        default = { };
        example = lib.literalExpression ''
          {
            listen = "10.0.0.5:8095";
            radarr = [{
              label = "Movies";
              url = "http://127.0.0.1:7878";
              api_key_file = "''${config.services.queuewatch.collect.credentialsDirectory}/radarr";
            }];
          }
        '';
        description = ''
          The `[collect]` section, as Nix. `token_file` is set by this module.
          Point every `api_key_file`/`password_file` into
          `credentialsDirectory`, under a name from `credentials`.
        '';
      };

      credentials = lib.mkOption {
        type = lib.types.attrsOf credentialType;
        default = { };
        example = { radarr = "/run/secrets/radarr-api-key"; };
        description = ''
          API keys and passwords, by the name the settings refer to them
          under. ${credentialDescription}
        '';
      };

      snapshotTokenFile = lib.mkOption {
        type = credentialType;
        description = "The bearer token the page must present. ${credentialDescription}";
      };

      credentialsDirectory = lib.mkOption {
        type = lib.types.str;
        readOnly = true;
        default = collectCredentials;
        description = "Where the collector finds its `credentials`.";
      };

      allowedClients = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "10.0.0.7" "fd00::/64" ];
        description = ''
          Addresses or CIDR ranges, besides `localhost`, the collector unit
          may exchange packets with. Empty (the default) means no restriction.
          When set, the unit gets `IPAddressDeny=any` and `IPAddressAllow=`
          `localhost` plus these.

          The snapshot carries requesters' e-mail addresses over plain HTTP,
          so the fewer machines can reach the listener, the better. But
          systemd filters BOTH directions: every service the collector polls
          must be on `localhost` or in this list too, or it shows as
          unreachable.
        '';
      };
    };

    serve = {
      enable = lib.mkEnableOption "the queuewatch page, which holds no API key";

      settings = lib.mkOption {
        type = settingsFormat.type;
        default = { };
        description = ''
          The `[serve]` section, as Nix. `snapshot_token_file`,
          `cookie_key_file` and `oidc.client_secret_file` are set by this
          module; everything else is yours. See the README for the keys.
        '';
      };

      snapshotTokenFile = lib.mkOption {
        type = credentialType;
        description = "The same token as the collector's. ${credentialDescription}";
      };

      cookieKeyFile = lib.mkOption {
        type = credentialType;
        description = "At least 64 random bytes, raw or base64. ${credentialDescription}";
      };

      oidcClientSecretFile = lib.mkOption {
        type = credentialType;
        description = "The OIDC client secret. ${credentialDescription}";
      };
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.collect.enable {
      assertions = [
        {
          # The token file is the module's to set; a value given in settings
          # would be silently replaced, and that is worse than a red build.
          assertion = !(cfg.collect.settings ? token_file);
          message = "services.queuewatch.collect.settings.token_file is set by the module; use snapshotTokenFile.";
        }
        {
          assertion = !(cfg.collect.credentials ? "snapshot-token");
          message = "services.queuewatch.collect.credentials: the name snapshot-token is reserved.";
        }
      ];

      users.users.queuewatch-collect = {
        isSystemUser = true;
        group = "queuewatch-collect";
      };
      users.groups.queuewatch-collect = { };

      systemd.services.queuewatch-collect = {
        description = "queuewatch — the collector";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        environment.QUEUEWATCH_CONFIG = "${collectFile}";
        serviceConfig = hardening "queuewatch-collect" // {
          ExecStart = "${lib.getExe cfg.package} collect";
          LoadCredential = [ "snapshot-token:${cfg.collect.snapshotTokenFile}" ]
            ++ lib.mapAttrsToList (name: source: "${name}:${source}") cfg.collect.credentials;
        } // lib.optionalAttrs (cfg.collect.allowedClients != [ ]) {
          IPAddressDeny = "any";
          IPAddressAllow = [ "localhost" ] ++ cfg.collect.allowedClients;
        };
      };
    })

    (lib.mkIf cfg.serve.enable {
      users.users.queuewatch-serve = {
        isSystemUser = true;
        group = "queuewatch-serve";
      };
      users.groups.queuewatch-serve = { };

      systemd.services.queuewatch-serve = {
        description = "queuewatch — the page";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        environment.QUEUEWATCH_CONFIG = "${serveFile}";
        # Discovery retries for a few minutes inside the program; beyond that
        # systemd restarts it, but not forever.
        startLimitBurst = 5;
        startLimitIntervalSec = 600;
        serviceConfig = hardening "queuewatch-serve" // {
          ExecStart = "${lib.getExe cfg.package} serve";
          LoadCredential = [
            "snapshot-token:${cfg.serve.snapshotTokenFile}"
            "cookie-key:${cfg.serve.cookieKeyFile}"
            "oidc-client-secret:${cfg.serve.oidcClientSecretFile}"
          ];
        };
      };
    })
  ];
}
