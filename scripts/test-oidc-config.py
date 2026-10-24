#!/usr/bin/env python3
"""Static regression for the checked-in Compose/realm wiring (stdlib only).

This checks the repository's block-mapping Compose layout, not general YAML or
runtime OIDC behavior. Run separately from the service-backed smoke tests.
"""
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


def mapping_block(text, key, indent):
    """Extract one block mapping at a known indentation; reject missing/duplicates."""
    lines = text.splitlines()
    marker = " " * indent + key + ":"
    starts = [i for i, line in enumerate(lines) if line == marker]
    if len(starts) != 1:
        raise AssertionError(f"expected exactly one {key!r} block at indent {indent}")
    result = []
    for line in lines[starts[0] + 1:]:
        if line.strip() and not line.lstrip().startswith("#"):
            if len(line) - len(line.lstrip()) <= indent:
                break
        result.append(line)
    return "\n".join(result)


class OidcConfigTest(unittest.TestCase):
    def test_realm_secret_is_passed_to_both_services(self):
        compose = (ROOT / "infra/docker-compose.yml").read_text()
        services = mapping_block(compose, "services", 0)
        realm = json.loads((ROOT / "infra/realm-pushit.json").read_text())
        clients = [c for c in realm["clients"] if c["clientId"] == "pushit-platform"]
        self.assertEqual(len(clients), 1)
        self.assertIs(clients[0]["publicClient"], False)
        self.assertEqual(clients[0]["secret"], "${PUSHIT_OIDC_CLIENT_SECRET}")
        for service in ("controller", "keycloak"):
            with self.subTest(service=service):
                block = mapping_block(services, service, 2)
                env = mapping_block(block, "environment", 4)
                # Optional interpolation preserves controller-only dev startup;
                # full-stack production must supply a nonempty shared secret.
                values = re.findall(r"^      PUSHIT_OIDC_CLIENT_SECRET:\s*(.*?)\s*$", env, re.M)
                self.assertEqual(values, ["${PUSHIT_OIDC_CLIENT_SECRET:-}"])
        keycloak = mapping_block(services, "keycloak", 2)
        self.assertIn("    command: start --import-realm", keycloak)
        self.assertIn("./realm-pushit.json:/opt/keycloak/data/import/realm-pushit.json:ro", keycloak)


if __name__ == "__main__":
    unittest.main()
