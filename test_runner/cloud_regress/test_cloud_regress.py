"""
Run the regression tests on the cloud instance of Neon
"""
import os
import re
import subprocess

import psycopg2
import pytest
from fixtures.log_helper import log
from fixtures.neon_fixtures import RemotePostgres


@pytest.mark.timeout(7200)
@pytest.mark.remote_cluster
def test_cloud_regress(remote_pg: RemotePostgres):
    """
    Run the regression tests
    """
    pg_version = re.search(r"\-pg(\d+)\]", os.environ.get('PYTEST_CURRENT_TEST')).group(1)
    conn = psycopg2.connect(remote_pg.connstr())
    cur = conn.cursor()
    cur.execute("SELECT COUNT(*) FROM pg_extension WHERE extname = 'regress_so'")
    num_ext = cur.fetchone()[0]
    assert int(num_ext) < 2
    if num_ext == 1:
        log.info('The extension is found')
    else:
        log.info('Creating the extension')
        cur.execute('CREATE EXTENSION regress_so')
        conn.commit()

    log.info('Creating a C function to check availability of regress.so')
    cur.execute("CREATE FUNCTION get_columns_length(oid[]) "
                "RETURNS int AS 'regress.so' LANGUAGE C STRICT STABLE PARALLEL SAFE;")
    conn.rollback()
    neondir = os.path.abspath(os.path.join(os.path.dirname(os.path.relpath(__file__)), '../../'))
    runpath = f'{neondir}/vendor/postgres-v{pg_version}/src/test/regress'
    artpath = '/tmp/neon'
    prefix = f'{artpath}/pg_install/v{pg_version}'
    binpath = f'{prefix}/bin'
    regress_bin = f'{prefix}/lib/postgresql/pgxs/src/test/regress/pg_regress'

    env_vars = {
       'PGHOST': remote_pg.default_options['host'],
       'PGPORT': str(remote_pg.default_options['port'] if 'port' in remote_pg.default_options else 5432),
       'PGUSER': remote_pg.default_options['user'],
       'PGPASSWORD': remote_pg.default_options['password'],
       'PGDATABASE': remote_pg.default_options['dbname']
    }
    regress_cmd = [
        regress_bin,
        '--inputdir=.',
        f"--bindir={binpath}",
        "--dlpath=/usr/local/lib",
        "--max-concurrent-tests=20",
        "--schedule=./parallel_schedule",
        "--max-connections=5"
    ]
    try:
        remote_pg.pg_bin.run(regress_cmd, env=env_vars, cwd=runpath)
    except subprocess.CalledProcessError as e:
        log.error('Error(s) occurred while running the regression tests')
        with open(f'{runpath}/regression.out', 'r') as f:
            print(f.read())
        with open(f'{runpath}/regression.diffs', 'r') as f:
            print(f.read())
        raise e
