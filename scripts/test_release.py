import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import release

SHA = 'a' * 40
VERSION = '0.2.0'


def draft(version=VERSION, sha=SHA):
    return dict(id=7, tag_name='v'+version, target_commitish=sha, draft=True, immutable=False,
                prerelease=False, body=release.marker(version, sha), author={'login':'github-actions[bot]'})


class API:
    def __init__(self, existing=None):
        self.releases = [] if existing is None else [existing]
        self.assets = []
        self.refs = []
        self.writes = []
        self.upload_failure = False
        self.corrupt_upload = False

    def pages(self, path):
        return copy.deepcopy(self.assets if '/assets?' in path else self.releases)

    def request(self, path, method='GET', data=None):
        if method != 'GET':
            self.writes.append((path, method, data))
        if path.startswith('git/matching-refs'):
            return self.refs
        if path == 'releases' and method == 'POST':
            self.releases = [dict(draft(), **data)]
        elif method == 'DELETE':
            self.assets = [a for a in self.assets if str(a['id']) != path.split('/')[-1]]
            return None
        elif method == 'PATCH':
            self.releases[0].update(data)
            if data.get('draft') is False:
                self.releases[0].update(immutable=True, html_url='https://example.test/release')
        return copy.deepcopy(self.releases[0])

    def upload(self, tag, files):
        if self.upload_failure:
            raise RuntimeError('simulated interrupted upload')
        self.assets = [dict(id=i, name=p.name, state='uploaded', size=p.stat().st_size,
                            digest='sha256:'+release.file_digest(p)) for i,p in enumerate(files)]
        if self.corrupt_upload:
            self.assets[0]['digest'] = 'sha256:wrong'


class ReleaseTests(unittest.TestCase):
    def test_changelog_requires_one_nonempty_matching_section(self):
        text = '# Changelog\n\n## [Unreleased]\nfuture\n## [0.2.0] - 2026-09-11\n### Added\n- Updates\n\n## [0.1.0]\nold\n'
        self.assertEqual(release.changelog_section(text, VERSION), '### Added\n- Updates\n')
        for invalid in ['', '## [0.2.0]\n### Added\n<!-- TODO -->', text+'\n## [0.2.0]\nduplicate']:
            with self.assertRaises(ValueError):
                release.changelog_section(invalid, VERSION)
        with self.assertRaises(ValueError):
            release.numeric_version('0.2.0-beta')

    def test_lock_version_must_match_manifest(self):
        with tempfile.TemporaryDirectory() as d:
            p=Path(d)
            (p/'Cargo.toml').write_text('[package]\nname="gopher"\nversion="0.2.0"\n')
            (p/'Cargo.lock').write_text('[[package]]\nname="gopher"\nversion="0.1.0"\n')
            (p/'CHANGELOG.md').write_text('## [0.2.0]\n- Updates\n')
            with self.assertRaisesRegex(ValueError, 'versions differ'):
                release.metadata(p)
            (p/'Cargo.lock').write_text('[[package]]\nname="gopher"\nversion="0.2.0"\n')
            self.assertEqual(release.metadata(p), (VERSION, '- Updates\n'))

    def test_first_release_and_numeric_ordering(self):
        self.assertIsNone(release.validate_remote(API(), VERSION, SHA))
        api=API(dict(draft('0.9.0'), draft=False))
        self.assertIsNone(release.validate_remote(api, '0.10.0', SHA))
        for version in ['0.9.0','0.8.1']:
            with self.assertRaisesRegex(ValueError, 'newer'):
                release.validate_remote(api, version, SHA)
        api.releases.append(dict(draft('9.0.0'), prerelease=True, draft=False))
        self.assertIsNone(release.validate_remote(api, '0.10.0', SHA))

    def test_api_failure_is_not_a_first_release(self):
        with patch.object(API, 'pages', side_effect=RuntimeError('API unavailable')):
            with self.assertRaisesRegex(RuntimeError, 'API unavailable'):
                release.validate_remote(API(), VERSION, SHA)

    def test_only_workflow_draft_at_exact_commit_can_be_recovered(self):
        self.assertEqual(release.validate_remote(API(draft()), VERSION, SHA)['id'], 7)
        for changed in [dict(draft(), target_commitish='b'*40), dict(draft(), body=''),
                        dict(draft(), author={'login':'someone'}), dict(draft(), immutable=True)]:
            with self.assertRaises(ValueError):
                release.validate_remote(API(changed), VERSION, SHA)
        api=API()
        api.refs=[{'ref':'refs/tags/v0.2.0','object':{'type':'commit','sha':SHA}}]
        with self.assertRaisesRegex(ValueError, 'Tag collision'):
            release.validate_remote(api, VERSION, SHA)
        api.releases=[draft()]
        self.assertEqual(release.validate_remote(api, VERSION, SHA)['id'],7)
        api.refs[0]['object']['sha']='b'*40
        with self.assertRaisesRegex(ValueError, 'Tag collision'):
            release.validate_remote(api, VERSION, SHA)

    def artifacts(self, directory):
        for name in release.artifact_names(VERSION):
            (directory/name).write_bytes(name.encode())
        name=release.artifact_names(VERSION)[0]
        (directory/(name+'.sha256')).write_text(f'{release.file_digest(directory/name)}  {name}\n')
        release.write_receipt(directory, VERSION, SHA)

    def test_publish_only_after_all_uploaded_hashes_match(self):
        with tempfile.TemporaryDirectory() as d:
            directory=Path(d);self.artifacts(directory)
            api=API()
            self.assertEqual(release.publish(api,directory,VERSION,SHA,'Notes\n'),'https://example.test/release')
            self.assertEqual(api.writes[-1][2], {'draft':False,'make_latest':'true'})
            self.assertFalse(api.releases[0]['draft'])
            with self.assertRaises(ValueError):
                release.publish(api,directory,VERSION,SHA,'Notes\n')

    def test_interrupted_and_corrupt_uploads_stay_drafts_and_are_recoverable(self):
        with tempfile.TemporaryDirectory() as d:
            directory=Path(d);self.artifacts(directory)
            api=API();api.upload_failure=True
            with self.assertRaises(RuntimeError):
                release.publish(api,directory,VERSION,SHA,'Notes\n')
            self.assertTrue(api.releases[0]['draft'])
            api.upload_failure=False;api.corrupt_upload=True
            with self.assertRaisesRegex(ValueError, 'incomplete'):
                release.publish(api,directory,VERSION,SHA,'Notes\n')
            self.assertTrue(api.releases[0]['draft'])
            api.corrupt_upload=False
            release.publish(api,directory,VERSION,SHA,'Notes\n')
            self.assertTrue(any(method=='DELETE' for _,method,_ in api.writes))

    def test_tampered_local_artifacts_and_unknown_remote_assets_are_not_published(self):
        with tempfile.TemporaryDirectory() as d:
            directory=Path(d);self.artifacts(directory)
            api=API(draft());api.assets=[{'id':1,'name':'unexpected'}]
            with self.assertRaisesRegex(ValueError, 'Unexpected draft asset'):
                release.publish(api,directory,VERSION,SHA,'Notes\n')
            self.assertEqual(api.writes,[])
            (directory/'install.sh').write_text('tampered')
            with self.assertRaisesRegex(ValueError, 'artifact'):
                release.publish(api,directory,VERSION,SHA,'Notes\n')
            self.assertEqual(api.writes,[])

    def test_reject_dispatch_from_other_repo_branch_event_or_checkout(self):
        env={'GITHUB_REPOSITORY':release.REPO,'GITHUB_REF':'refs/heads/main',
             'GITHUB_EVENT_NAME':'workflow_dispatch','GITHUB_SHA':SHA}
        for changes in [{'GITHUB_REF':'refs/heads/feature/test'}, {'GITHUB_EVENT_NAME':'push'},
                        {'GITHUB_REPOSITORY':'someone/fork'}, {'GITHUB_SHA':'bad'}]:
            with patch.dict('os.environ',dict(env,**changes),clear=True), self.assertRaises(ValueError):
                release.workflow_commit()
        with patch.dict('os.environ',env,clear=True), patch.object(release,'run',return_value='b'*40):
            with self.assertRaisesRegex(ValueError,'Checkout'):
                release.workflow_commit()
