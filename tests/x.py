#!/usr/bin/env python3

import subprocess
import os
import sys
import time
import argparse
import platform
import urllib.request
from glob import glob
from docker import start_database

parser = argparse.ArgumentParser()
parser.add_argument("-t", "--target")
parser.add_argument("-e", "--target-exact")
parser.add_argument("-l", "--list-targets", action="store_true")
parser.add_argument("--test")
parser.add_argument("--clippy", action="store_true")

argv, unknown = parser.parse_known_args()

_list_targets_seen = set()

# base dir of sqlx workspace
dir_workspace = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))

# dir of tests
dir_tests = os.path.join(dir_workspace, "tests")

RUNTIMES = ["async-std", "async-global-executor", "smol", "tokio"]
CHECK_TLS = ["native-tls", "rustls", "rustls-ring", "rustls-aws-lc-rs", "none"]
TLS_VARIANTS = ["native-tls", "rustls-ring", "rustls-aws-lc-rs", "none"]
POSTGRES_VERSIONS = ["17", "16", "15", "14", "13"]
MYSQL_VERSIONS = ["8", "5_7"]
MARIADB_VERSIONS = ["verylatest", "11_8", "11_4", "10_11", "10_6"]


def maybe_fetch_sqlite_extension():
    """
    For supported platforms, if we're testing SQLite and the file isn't
    already present, grab a simple extension for testing.

    Returns the extension name if it was downloaded successfully or `None` if not.
    """
    BASE_URL = "https://github.com/nalgeon/sqlean/releases/download/0.15.2/"
    if platform.system() == "Darwin":
        if platform.machine() == "arm64":
            download_url = BASE_URL + "/ipaddr.arm64.dylib"
            filename = "ipaddr.dylib"
        else:
            download_url = BASE_URL + "/ipaddr.dylib"
            filename = "ipaddr.dylib"
    elif platform.system() == "Linux":
        download_url = BASE_URL + "/ipaddr.so"
        filename = "ipaddr.so"
    else:
        # Unsupported OS
        return None

    if not os.path.exists(filename):
        content = urllib.request.urlopen(download_url).read()
        with open(filename, "wb") as fd:
            fd.write(content)

    return filename.split(".")[0]


def required_feature_for_test(test_name):
    for feature in ["postgres", "mysql", "sqlite", "any"]:
        if test_name.startswith(feature):
            return feature
    return None


def extract_features(command):
    tokens = command.split(" ")
    for i, token in enumerate(tokens):
        if token == "--features" and i + 1 < len(tokens):
            return set(tokens[i + 1].split(","))
    return None


def core_tls_features(tls):
    if tls == "rustls":
        return ["_tls-rustls-ring-webpki"]
    if tls == "rustls-ring":
        return ["_tls-rustls-ring-webpki", "_tls-rustls-ring-native-roots"]
    if tls == "rustls-aws-lc-rs":
        return ["_tls-rustls-aws-lc-rs"]
    return [f"_tls-{tls}"]


def run(command, comment=None, env=None, service=None, tag=None, args=None, database_url_args=None):
    if argv.list_targets:
        if tag:
            if tag not in _list_targets_seen:
                print(f"{tag}")
                _list_targets_seen.add(tag)

        return

    if argv.target and not tag.startswith(argv.target):
        return

    if argv.target_exact and tag != argv.target_exact:
        return

    if comment is not None:
        print(f"\x1b[2m # {comment}\x1b[0m")

    environ = env or {}

    if service == "sqlite":
        if maybe_fetch_sqlite_extension() is not None:
            if environ.get("RUSTFLAGS"):
                environ["RUSTFLAGS"] += " --cfg sqlite_ipaddr"
            else:
                environ["RUSTFLAGS"] = "--cfg sqlite_ipaddr"
            if platform.system() == "Linux":
                if os.environ.get("LD_LIBRARY_PATH"):
                    environ["LD_LIBRARY_PATH"]= os.environ.get("LD_LIBRARY_PATH") + ":"+ os.getcwd()
                else:
                    environ["LD_LIBRARY_PATH"]=os.getcwd()


    if service is not None:
        database_url = start_database(service, database="sqlite/sqlite.db" if service == "sqlite" else "sqlx", cwd=dir_tests)

        if database_url_args:
            database_url += "?" + database_url_args

        environ["DATABASE_URL"] = database_url

        # show the database url
        print(f"\x1b[94m @ {database_url}\x1b[0m")

    command_args = []

    if argv.test:
        if command.startswith("cargo c") or command.startswith("cargo check") or command.startswith("cargo clippy"):
            return
        if "--manifest-path" in command:
            return
        required = required_feature_for_test(argv.test)
        if required is not None:
            features = extract_features(command)
            if features is None or (required not in features and "all-databases" not in features):
                return
        if command.startswith("cargo test"):
            command_args.extend(["--test", argv.test])

    if unknown:
        command_args.extend(["--", *unknown])

        if args is not None:
            command_args.extend(args)

    print(f"\x1b[93m $ {command} {' '.join(command_args)}\x1b[0m")

    cwd = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
    res = subprocess.run(
        [
            *command.split(" "),
            *command_args
        ],
        env=dict(list(os.environ.items()) + list(environ.items())),
        cwd=cwd,
    )

    if res.returncode != 0:
        sys.exit(res.returncode)


def postgres_env(version):
    env = {}
    rustflags = os.environ.get("RUSTFLAGS", "").strip()
    version_flag = f'--cfg postgres="{version}"'
    if rustflags:
        env["RUSTFLAGS"] = f"{rustflags} {version_flag}"
    else:
        env["RUSTFLAGS"] = version_flag
    return env


# before we start, we clean previous profile data
# keeping these around can cause weird errors
for path in glob(os.path.join(os.path.dirname(__file__), "target/**/*.gc*"), recursive=True):
    os.remove(path)

#
# check
#

CHECK_CMD = "cargo clippy" if argv.clippy else "cargo c"

for runtime in RUNTIMES:
    for tls in CHECK_TLS:
        run(
            f"{CHECK_CMD} --no-default-features --features all-databases,_unstable-all-types,macros,sqlite-preupdate-hook,runtime-{runtime},tls-{tls}",
            comment=f"check {runtime} {tls}",
            tag=f"check_{runtime}_{tls}",
        )

if argv.clippy:
    sys.exit(0)

#
# unit test
#

for runtime in RUNTIMES:
    for tls in TLS_VARIANTS:
        core_features = [
            "json",
            "offline",
            "migrate",
            "sqlx-toml",
            f"_rt-{runtime}",
            *core_tls_features(tls),
        ]
        run(
            "cargo test --no-default-features --manifest-path sqlx-core/Cargo.toml "
            f"--features {','.join(core_features)}",
            comment=f"unit test core {runtime} {tls}",
            tag=f"unit_{runtime}_{tls}",
        )

run(
    "cargo test -p sqlx-mysql --no-default-features --features rsa --lib",
    comment="unit test sqlx-mysql rsa",
    tag="unit_mysql_rsa",
)

#
# integration tests
#

for runtime in RUNTIMES:
    for tls in TLS_VARIANTS:
        #
        # sqlite
        #

        run(
            f"cargo test --no-default-features "
            f"--features any,sqlite,macros,migrate,sqlite-preupdate-hook,_unstable-all-types,runtime-{runtime},tls-{tls}",
            comment="test sqlite",
            env={"RUST_TEST_THREADS": "1"},
            service="sqlite",
            tag=f"sqlite_{runtime}",
        )

        #
        # postgres
        #

        for version in POSTGRES_VERSIONS:
            run(
                f"cargo test --no-default-features "
                f"--features any,postgres,macros,migrate,_unstable-all-types,runtime-{runtime},tls-{tls}",
                comment=f"test postgres {version}",
                env=postgres_env(version),
                service=f"postgres_{version}",
                tag=f"postgres_{version}_{runtime}",
            )

            if tls != "none":
                ## +ssl
                run(
                    f"cargo test --no-default-features "
                    f"--features any,postgres,macros,migrate,_unstable-all-types,runtime-{runtime},tls-{tls}",
                    comment=f"test postgres {version} ssl",
                    database_url_args="sslmode=verify-ca&sslrootcert=.%2Ftests%2Fcerts%2Fca.crt",
                    env=postgres_env(version),
                    service=f"postgres_{version}",
                    tag=f"postgres_{version}_ssl_{runtime}",
                )

                ## +client-ssl
                run(
                    f"cargo test --no-default-features "
                    f"--features any,postgres,macros,migrate,_unstable-all-types,runtime-{runtime},tls-{tls}",
                    comment=f"test postgres {version}_client_ssl no-password",
                    database_url_args="sslmode=verify-ca&sslrootcert=.%2Ftests%2Fcerts%2Fca.crt&sslkey=.%2Ftests%2Fcerts%2Fkeys%2Fclient.key&sslcert=.%2Ftests%2Fcerts%2Fclient.crt",
                    env=postgres_env(version),
                    service=f"postgres_{version}_client_ssl",
                    tag=f"postgres_{version}_client_ssl_no_password_{runtime}",
                )

        #
        # mysql
        #

        for version in MYSQL_VERSIONS:
            base_features = f"any,mysql,macros,migrate,_unstable-all-types,runtime-{runtime},tls-{tls}"
            rsa_features = f"any,mysql,mysql-rsa,macros,migrate,_unstable-all-types,runtime-{runtime},tls-{tls}"
            features = rsa_features if tls == "none" else base_features
            base_url_args = "ssl-mode=disabled" if tls == "none" else "ssl-mode=required"
            client_ssl_ca = ".%2Ftests%2Fcerts%2Fca.crt"
            client_ssl_key = ".%2Ftests%2Fcerts%2Fkeys%2Fclient.key"
            client_ssl_cert = ".%2Ftests%2Fcerts%2Fclient.crt"
            if version == "5_7":
                # MySQL 5.7 cannot load Ed25519 certs; use the RSA set for client-SSL targets.
                client_ssl_ca = ".%2Ftests%2Fcerts%2Frsa%2Fca.crt"
                client_ssl_key = ".%2Ftests%2Fcerts%2Frsa%2Fkeys%2Fclient.key"
                client_ssl_cert = ".%2Ftests%2Fcerts%2Frsa%2Fclient.crt"
            client_ssl_args = (
                f"sslmode=verify_ca&ssl-ca={client_ssl_ca}"
                f"&ssl-key={client_ssl_key}&ssl-cert={client_ssl_cert}"
            )

            # Since docker mysql 5.7 using yaSSL(It only supports TLSv1.1), avoid running when using rustls.
            # https://github.com/docker-library/mysql/issues/567
            # only run when using native-tls
            if not (version == "5_7" and tls in ["rustls-ring", "rustls-aws-lc-rs"]):
                run(
                    f"cargo test --no-default-features --features {features}",
                    comment=f"test mysql {version}",
                    database_url_args=base_url_args,
                    service=f"mysql_{version}",
                    tag=f"mysql_{version}_{runtime}",
                )

                ## +client-ssl
                if tls != "none" and not (version == "5_7" and tls in ["rustls-ring", "rustls-aws-lc-rs"]):
                    run(
                        f"cargo test --no-default-features --features {base_features}",
                        comment=f"test mysql {version}_client_ssl no-password",
                        database_url_args=client_ssl_args,
                        service=f"mysql_{version}_client_ssl",
                        tag=f"mysql_{version}_client_ssl_no_password_{runtime}",
                    )

            if tls == "native-tls" and runtime == "tokio" and version == "8":
                run(
                    f"cargo test --no-default-features --features {rsa_features}",
                    comment=f"test mysql {version} tls with rsa",
                    database_url_args="ssl-mode=required",
                    service=f"mysql_{version}",
                    tag=f"mysql_{version}_tls_rsa_{runtime}",
                )

        #
        # mariadb
        #

        for version in MARIADB_VERSIONS:
            base_features = f"any,mysql,macros,migrate,_unstable-all-types,runtime-{runtime},tls-{tls}"
            rsa_features = f"any,mysql,mysql-rsa,macros,migrate,_unstable-all-types,runtime-{runtime},tls-{tls}"
            features = rsa_features if tls == "none" else base_features
            base_url_args = "ssl-mode=disabled" if tls == "none" else "ssl-mode=required"

            run(
                f"cargo test --no-default-features --features {features}",
                comment=f"test mariadb {version}",
                database_url_args=base_url_args,
                service=f"mariadb_{version}",
                tag=f"mariadb_{version}_{runtime}",
            )

            ## +client-ssl
            if tls != "none":
                run(
                    f"cargo test --no-default-features --features {base_features}",
                    comment=f"test mariadb {version}_client_ssl no-password",
                    database_url_args="sslmode=verify_ca&ssl-ca=.%2Ftests%2Fcerts%2Fca.crt&ssl-key=%2Ftests%2Fcerts%2Fkeys%2Fclient.key&ssl-cert=.%2Ftests%2Fcerts%2Fclient.crt",
                    service=f"mariadb_{version}_client_ssl",
                    tag=f"mariadb_{version}_client_ssl_no_password_{runtime}",
                )

            if tls == "native-tls" and runtime == "tokio" and version == "10_11":
                run(
                    f"cargo test --no-default-features --features {rsa_features}",
                    comment=f"test mariadb {version} tls with rsa",
                    database_url_args="ssl-mode=required",
                    service=f"mariadb_{version}",
                    tag=f"mariadb_{version}_tls_rsa_{runtime}",
                )

# TODO: Use [grcov] if available
# ~/.cargo/bin/grcov tests/.cache/target/debug -s sqlx-core/ -t html --llvm --branch -o ./target/debug/coverage
