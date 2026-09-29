"""Exercise release preparation through its public CLI and a local Git remote."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


PROJECT = Path(__file__).resolve().parents[1]


class ReleasePreparation(unittest.TestCase):
    """No test contacts a registry or publishes a package."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.env = os.environ.copy()
        self.env.update({
            'GITHUB_REF_TYPE': 'branch',
            'GITHUB_REF_NAME': 'main',
            'GITHUB_OUTPUT': str(self.root / 'output'),
            'GIT_CONFIG_GLOBAL': os.devnull,
            'GIT_CONFIG_NOSYSTEM': '1',
        })
        self.git('init', '--bare', str(self.root / 'remote.git'))
        self.git('init', '-b', 'main')
        self.git('config', 'user.name', 'Release test')
        self.git('config', 'user.email', 'release@example.com')
        self.git('remote', 'add', 'origin', str(self.root / 'remote.git'))
        for name in (
            'scripts/prepare-release.sh', 'scripts/check.sh', '.githooks/commit-msg',
            'release-plz.toml',
        ):
            target = self.repo / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(PROJECT / name, target)
        (self.repo / 'src').mkdir()
        (self.repo / 'src/lib.rs').write_text('pub fn example() {}\n')
        self.write_version('0.1.0')
        self.commit('feat: initial crate')
        self.git('tag', 'v0.1.0')
        self.git('push', '-u', 'origin', 'main', '--tags')
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.env['PATH'] = str(self.bin) + os.pathsep + self.env['PATH']
        self.executable('cargo', '''#!/usr/bin/env python3
import json, os, pathlib, re, sys
if sys.argv[1] == 'metadata':
    version = re.search(r'version = "([^"]+)"', pathlib.Path('Cargo.toml').read_text())[1]
    print(json.dumps({'packages': [{'version': version}]}))
elif sys.argv[1:] == ['publish', '--dry-run', '--locked', '--all-features']:
    pathlib.Path('dry-run-observed').write_text('verified')
    sys.exit(int(os.environ.get('FAIL_DRY_RUN', '0')))
else:
    sys.exit('Unexpected Cargo command')
''')
        self.executable('release-plz', '''#!/usr/bin/env python3
import re, sys
from pathlib import Path
body = re.search(r'body = """(.*?)"""', Path('release-plz.toml').read_text(), re.S)
if body is None:
    sys.exit('release-plz.toml sets no changelog body')
heading = re.sub(r'[{][{]\\s*version\\s*[}][}]', '0.1.1', body[1])
heading = re.sub(r'[{][{]\\s*timestamp[^}]*[}][}]', '2026-09-28', heading)
if '{' in heading:
    sys.exit('this stand-in renders only the version and the date')
for name in ('Cargo.toml', 'Cargo.lock'):
    file = Path(name)
    file.write_text(file.read_text().replace('0.1.0', '0.1.1'))
changelog = Path('CHANGELOG.md')
header, entries = changelog.read_text().split('## [Unreleased]\\n', 1)
changelog.write_text(header + '## [Unreleased]\\n' + heading + entries)
''')

    def git(self, *args):
        return subprocess.check_output(
            ['git', *args], cwd=self.repo, env=self.env,
            stderr=subprocess.DEVNULL, text=True,
        ).strip()

    def executable(self, name, body):
        path = self.bin / name
        path.write_text(body)
        path.chmod(0o755)

    def write_version(self, version):
        (self.repo / 'Cargo.toml').write_text('[package]\nversion = "' + version + '"\n')
        (self.repo / 'Cargo.lock').write_text('version = "' + version + '"\n')
        (self.repo / 'CHANGELOG.md').write_text(
            '# Changelog\n\n## [Unreleased]\n\n### Fixed\n\n- An entry.\n\n'
            '## [' + version + '] - 2026-09-27\n'
        )

    def commit(self, message, push=False):
        self.git('add', '.')
        self.git('commit', '-m', message)
        if push:
            self.git('push', 'origin', 'main')

    def run_prepare(self):
        return subprocess.run(
            ['bash', 'scripts/prepare-release.sh'], cwd=self.repo, env=self.env,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )

    def test_documentation_change_does_not_release(self):
        (self.repo / 'README.md').write_text('Documentation update\n')
        self.commit('docs: explain the crate', push=True)
        result = self.run_prepare()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertFalse((self.root / 'output').exists())

    def test_source_change_prepares_and_pushes_one_version(self):
        (self.repo / 'src/lib.rs').write_text('pub fn example() { println!("updated"); }\n')
        self.commit('fix: update behaviour', push=True)
        result = self.run_prepare()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn('tag=v0.1.1', (self.root / 'output').read_text())
        self.assertEqual(self.git('rev-parse', 'HEAD'), self.git('rev-parse', 'origin/main'))
        self.assertEqual(self.git('branch', '--show-current'), 'chore/release-0.1.1')
        self.assertTrue((self.repo / 'dry-run-observed').exists())

    def test_failed_package_verification_does_not_push(self):
        (self.repo / 'src/lib.rs').write_text('pub fn updated() {}\n')
        self.commit('feat: update the API', push=True)
        before = self.git('rev-parse', 'origin/main')
        self.env['FAIL_DRY_RUN'] = '1'
        result = self.run_prepare()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.git('rev-parse', 'origin/main'), before)
        self.assertFalse((self.root / 'output').exists())

    def test_stale_commit_does_not_release(self):
        before = self.git('rev-parse', 'HEAD')
        (self.repo / 'src/lib.rs').write_text('pub fn updated() {}\n')
        self.commit('feat: update the API', push=True)
        self.git('checkout', before)
        result = self.run_prepare()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertFalse((self.root / 'output').exists())

    def test_manual_tag_keeps_the_selected_version(self):
        self.env.update({'GITHUB_REF_TYPE': 'tag', 'GITHUB_REF_NAME': 'v0.1.0'})
        result = self.run_prepare()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn('tag=v0.1.0', (self.root / 'output').read_text())
