"""Exercise Git Bash detection without running the installer or writing the user Path."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


@unittest.skipUnless(os.name == "nt", "PowerShell installer is Windows-only")
class GitBashDetection(unittest.TestCase):
    def test_search_order_and_malformed_path_entries(self):
        windows = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
        shells = [windows] if windows.is_file() else []
        if pwsh := shutil.which("pwsh"):
            shells.append(Path(pwsh))
        if not shells:
            self.skipTest("PowerShell is not installed")
        installer = Path(__file__).resolve().parents[2] / "scripts/install.ps1"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            programs = root / "Program Files"
            git = programs / "Git"
            (git / "bin").mkdir(parents=True)
            (git / "cmd").mkdir()
            (git / "bin/bash.exe").touch()
            (git / "cmd/git.exe").touch()
            empty = root / "empty"
            empty.mkdir()
            script = root / "detect.ps1"
            script.write_text('''param([string]$Installer, [string]$CaseFile)
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
$function = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Find-GitBash' }, $true)
Invoke-Expression $function.Extent.Text
$case = Get-Content -Raw -LiteralPath $CaseFile | ConvertFrom-Json
$env:Path = $case.path
$env:ProgramFiles = $case.program_files
$env:TIECHUI_GIT_BASH_PATH = $case.override
$gitBash = try { Find-GitBash } catch { $false }
ConvertTo-Json -InputObject ([bool]$gitBash) -Compress
''', encoding="utf-8")
            cases = [
                ("override", "", empty, git / "bin/bash.exe", True),
                ("missing override", git / "bin", programs, root / "missing.exe", False),
                ("bin", git / "bin", empty, "", True),
                ("WSL", Path(os.environ["SystemRoot"]) / "System32", empty, "", False),
                ("cmd", git / "cmd", empty, "", True),
                ("ProgramFiles", "", programs, "", True),
                ("malformed only", r"C:\bad|entry", empty, "", False),
                ("malformed before bin", f"C:\\bad|entry;{git / 'bin'}", empty, "", True),
                ("malformed before cmd", f"C:\\bad<entry;{git / 'cmd'}", empty, "", True),
            ]
            for shell in shells:
                for index, (name, path, program_files, override, expected) in enumerate(cases):
                    with self.subTest(shell=str(shell), case=name):
                        scenario = root / str(index)
                        scenario.mkdir(exist_ok=True)
                        home = scenario / "home"
                        home.mkdir(exist_ok=True)
                        case = scenario / "case.json"
                        case.write_text(json.dumps({"path": str(path), "program_files": str(program_files),
                                                    "override": str(override)}), encoding="utf-8")
                        environment = dict(os.environ, TIECHUI_HOME=str(home))
                        result = subprocess.run([shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script,
                                                 "-Installer", installer, "-CaseFile", case], cwd=scenario,
                                                env=environment, capture_output=True, timeout=30)
                        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
                        self.assertEqual(json.loads(result.stdout), expected)


if __name__ == "__main__":
    unittest.main()
