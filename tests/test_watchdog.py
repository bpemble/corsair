"""Watchdog escalation tests.

The escalation path is single-tier: when in-engine recovery fails N
consecutive times, the watchdog calls escalate_gateway_recreate() which
performs a full container teardown + volume wipe + recreate.
"""

import pytest

from src import watchdog
from src.watchdog import RECOVERY_FAILS_BEFORE_GATEWAY_RESTART


def test_attrs_to_run_kwargs_minimal():
    """Smoke test that the inspect-to-run config converter works on a
    minimal container attrs payload."""
    attrs = {
        'Config': {
            'Image': 'foo:latest',
            'Env': ['A=1', 'B=2'],
            'Labels': {'com.docker.compose.service': 'ib-gateway'},
            'Healthcheck': None,
        },
        'HostConfig': {
            'NetworkMode': 'host',
            'RestartPolicy': {'Name': 'unless-stopped', 'MaximumRetryCount': 0},
        },
        'Mounts': [],
        'Name': '/corsair2-ib-gateway-1',
    }
    kwargs = watchdog._gateway_attrs_to_run_kwargs(attrs)
    assert kwargs['image'] == 'foo:latest'
    assert kwargs['name'] == 'corsair2-ib-gateway-1'
    assert kwargs['network_mode'] == 'host'
    assert kwargs['environment'] == ['A=1', 'B=2']
    assert kwargs['labels']['com.docker.compose.service'] == 'ib-gateway'
    assert kwargs['restart_policy']['Name'] == 'unless-stopped'


def test_attrs_to_run_kwargs_with_volume_and_healthcheck():
    """Verify volume binds and healthcheck conversion (TitleCase → lowercase)."""
    attrs = {
        'Config': {
            'Image': 'foo:latest',
            'Env': [],
            'Labels': {},
            'Healthcheck': {
                'Test': ['CMD-SHELL', 'true'],
                'Interval': 10_000_000_000,
                'Timeout': 5_000_000_000,
                'Retries': 3,
                'StartPeriod': 90_000_000_000,
            },
        },
        'HostConfig': {
            'NetworkMode': 'host',
            'RestartPolicy': {'Name': 'no'},
        },
        'Mounts': [
            {'Type': 'volume', 'Name': 'corsair2_ib-gateway-data',
             'Destination': '/opt/ibgateway', 'RW': True},
            {'Type': 'bind', 'Source': '/host', 'Destination': '/in_container'},
        ],
        'Name': '/corsair2-ib-gateway-1',
    }
    kwargs = watchdog._gateway_attrs_to_run_kwargs(attrs)
    assert kwargs['volumes'] == {
        'corsair2_ib-gateway-data': {'bind': '/opt/ibgateway', 'mode': 'rw'},
    }
    # bind mount should NOT be in volume_binds
    assert len(kwargs['volumes']) == 1
    assert kwargs['healthcheck']['test'] == ['CMD-SHELL', 'true']
    assert kwargs['healthcheck']['interval'] == 10_000_000_000
    # restart_policy = 'no' is stripped (no policy)
    assert 'restart_policy' not in kwargs


def test_escalate_handles_missing_docker_sdk(monkeypatch):
    """If the docker SDK isn't installed, escalate_gateway_recreate should
    return False gracefully — not crash the watchdog."""
    import sys
    import builtins
    real_import = builtins.__import__

    def fake_import(name, *args, **kwargs):
        if name == 'docker':
            raise ImportError("simulated missing docker SDK")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, '__import__', fake_import)
    assert watchdog.escalate_gateway_recreate() is False
