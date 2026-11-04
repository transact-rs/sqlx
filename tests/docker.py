import subprocess
import sys
import time
from os import path
import shutil

# base dir of sqlx workspace
dir_workspace = path.dirname(path.dirname(path.realpath(__file__)))

# dir of tests
dir_tests = path.join(dir_workspace, "tests")


# start database server and return a URL to use to connect
def docker_compose_command():
    if shutil.which("docker-compose"):
        return ["docker-compose"]
    if shutil.which("docker"):
        return ["docker", "compose"]
    return None


def start_database(driver, database, cwd):
    if driver == "sqlite":
        database = path.join(cwd, database)
        (base_path, ext) = path.splitext(database)
        new_database = f"{base_path}.test{ext}"
        if path.exists(database):
            shutil.copy(database, new_database)
        # short-circuit for sqlite
        return f"sqlite://{path.join(cwd, new_database)}?mode=rwc"

    compose_cmd = docker_compose_command()
    if compose_cmd is None:
        raise FileNotFoundError("docker-compose or docker compose not found")

    compose_args = [*compose_cmd, "-p", "sqlx"]
    res = subprocess.run(
        [*compose_args, "up", "-d", driver],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=dir_tests,
    )

    if res.returncode != 0:
        print(res.stderr, file=sys.stderr)

    if b"done" in res.stderr:
        time.sleep(30)

    res = subprocess.run(
        [*compose_args, "ps", "-q", driver],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=dir_tests,
    )

    if res.returncode != 0:
        print(res.stderr, file=sys.stderr)
        raise RuntimeError(f"failed to resolve container for {driver}")

    container_id = res.stdout.strip().decode()
    if not container_id:
        raise RuntimeError(f"no container found for {driver}")

    # determine appropriate port for driver
    if driver.startswith("mysql") or driver.startswith("mariadb"):
        port = 3306

    elif driver.startswith("postgres"):
        port = 5432

    else:
        raise NotImplementedError

    # find port
    format_arg = f"{{{{(index (index .NetworkSettings.Ports \"{port}/tcp\") 0).HostPort}}}}"
    res = subprocess.run(
        ["docker", "inspect", "-f", format_arg, container_id],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=dir_tests,
    )

    if res.returncode != 0:
        print(res.stderr, file=sys.stderr)

    port = int(res.stdout.decode().strip())

    # need additional permissions to connect to MySQL when using SSL
    if driver.startswith("mysql") or driver.startswith("mariadb"):
        mysql_args = ["docker", "exec", container_id, "mysql", "-u", "root"]
        if not driver.endswith("client_ssl"):
            mysql_args.append("-ppassword")
        mysql_args.extend(["-e", "GRANT ALL PRIVILEGES ON *.* TO 'root' WITH GRANT OPTION;"])
        res = subprocess.run(
            mysql_args,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=dir_tests,
        )

        if res.returncode != 0:
            print(res.stderr, file=sys.stderr)

    # do not set password in URL if authenticating using SSL key file
    if driver.endswith("client_ssl"):
        password = ""
    else:
        password = ":password"

    # construct appropriate database URL
    if driver.startswith("mysql") or driver.startswith("mariadb"):
        return f"mysql://root{password}@localhost:{port}/{database}"

    elif driver.startswith("postgres"):
        return f"postgres://postgres{password}@localhost:{port}/{database}"

    else:
        raise NotImplementedError
