import base64
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import release_signing as signing


class SigningTests(unittest.TestCase):
    def test_command_errors_do_not_expose_secrets_or_stderr(self):
        failed=subprocess.CompletedProcess([],1,'','secret-value')
        with patch.object(signing.subprocess,'run',return_value=failed):
            with self.assertRaises(ValueError) as error:
                signing.command('security','-p','secret-value')
            self.assertNotIn('secret-value',str(error.exception))

    def test_cleanup_removes_plain_key_even_if_keychain_cleanup_fails(self):
        with tempfile.TemporaryDirectory() as d, patch.dict(os.environ,{'RUNNER_TEMP':d}):
            root,keychain,certificate,key=signing.paths();root.mkdir()
            for p in (keychain,certificate,key):p.write_text('fixture')
            with patch.object(signing,'command',side_effect=ValueError('failed')):
                with self.assertRaises(ValueError):signing.cleanup()
            self.assertFalse(key.exists());self.assertFalse(certificate.exists())

    def test_import_exports_only_identity_and_key_path_to_workflow(self):
        with tempfile.TemporaryDirectory() as d:
            output=Path(d)/'env'
            env={'RUNNER_TEMP':d,'GITHUB_ENV':str(output),
                 'GOPHER_APPLE_CERTIFICATE_P12':base64.b64encode(b'fixture').decode(),
                 'GOPHER_APPLE_CERTIFICATE_PASSWORD':'password-secret',
                 'GOPHER_SPARKLE_PRIVATE_KEY':base64.b64encode(bytes(32)).decode()}
            def command(*args):
                if args[1]=='list-keychains' and '-s' not in args:return '"/tmp/login.keychain-db"\n'
                if args[1]=='find-identity':return '1) '+'A'*40+' "Apple Development: Test"'
                return ''
            with patch.dict(os.environ,env), patch.object(signing,'command',side_effect=command):
                signing.setup()
                self.assertNotIn('password-secret',output.read_text())
                self.assertNotIn(env['GOPHER_SPARKLE_PRIVATE_KEY'],output.read_text())
                root,keychain,certificate,key=signing.paths()
                self.assertFalse(certificate.exists())
                self.assertEqual(key.stat().st_mode & 0o777,0o600)
                self.assertEqual(root.stat().st_mode & 0o777,0o700)
                signing.cleanup()
