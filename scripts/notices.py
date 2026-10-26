"""Write the third-party notices for the Chunker executables.
Run: python3 scripts/notices.py THIRD-PARTY-NOTICES

Crate licenses come from cargo-about, configured in about.toml. The Rust
standard library's notices come from the toolchain that runs this script.
"""
import html.parser
import json
from pathlib import Path
import subprocess
import sys

HEADER = """\
Third-party notices for Chunker

The Chunker executables are built with the open-source software listed below.
Each section names a license and the crates distributed under it, then gives
the license text. The last section reproduces the notices that the Rust
toolchain ships for its standard library, which every Rust executable includes.
"""
RULE = '*' * 80


def run(*args):
    result = subprocess.run(args, stdout=subprocess.PIPE, encoding='utf-8', check=False)
    if result.returncode:
        sys.exit(result.returncode)
    return result.stdout


def crate_licenses():
    # The check below and the output read one cargo-about run. A clarification
    # whose file cannot be fetched only logs a warning and leaves the crate with
    # the canonical text, so a separate run can differ from the checked one.
    licenses = json.loads(run('cargo', 'about', 'generate', '--locked', '--fail', '--format', 'json'))['licenses']
    # cargo-about substitutes the canonical license text when a crate's own file
    # is missing or unrecognized. The canonical Apache-2.0 text is the whole
    # license, but the other accepted licenses require the crate's copyright
    # line, which only its own file has.
    missing = sorted({
        f"{user['crate']['name']} {user['crate']['version']} ({lic['id']})"
        for lic in licenses
        if lic['source_path'] is None and lic['id'] != 'Apache-2.0'
        for user in lic['used_by']
    })
    if missing:
        sys.exit('No license file with a copyright notice for:\n  ' + '\n  '.join(missing)
                 + '\nAdd a clarification to about.toml, or fix the one a warning above names.')
    return licenses


def crate_section(lic):
    used_by = ''
    for user in lic['used_by']:
        crate = user['crate']
        source = f" ({crate['repository']})" if crate['repository'] else ''
        used_by += f"- {crate['name']} {crate['version']}{source}\n"
    return f"\n{RULE}\n{lic['name']}\n\nUsed by:\n{used_by}{RULE}\n\n{lic['text']}\n"


class PlainText(html.parser.HTMLParser):
    """The text of an HTML page, one paragraph per block element. Text inside
    <pre> keeps its line breaks and indentation."""

    BLOCKS = {'br', 'details', 'div', 'h1', 'h2', 'h3', 'li', 'p', 'pre', 'summary', 'ul'}

    def __init__(self):
        super().__init__()
        self.paragraphs = []
        self.pieces = []
        self.in_pre = False
        self.in_title = False

    def end_paragraph(self):
        text = ''.join(self.pieces)
        self.pieces = []
        if self.in_pre:
            # HTML drops a line break that directly follows <pre>.
            lines = text.removeprefix('\n').rstrip().split('\n')
            text = '\n'.join(line.rstrip() for line in lines)
        else:
            text = ' '.join(text.split())
        if text:
            self.paragraphs.append(text)

    def handle_starttag(self, tag, attrs):
        if tag in self.BLOCKS:
            self.end_paragraph()
        self.in_pre = self.in_pre or tag == 'pre'
        self.in_title = self.in_title or tag == 'title'

    def handle_endtag(self, tag):
        if tag in self.BLOCKS:
            self.end_paragraph()
        self.in_pre = self.in_pre and tag != 'pre'
        self.in_title = self.in_title and tag != 'title'

    def handle_data(self, data):
        # The title repeats the page's first heading.
        if not self.in_title:
            self.pieces.append(data)

    def text(self):
        self.close()
        self.end_paragraph()
        return '\n\n'.join(self.paragraphs) + '\n'


def standard_library_section():
    # Every executable links the standard library, which Cargo.lock does not
    # list. The rustc component ships its notices.
    sysroot = run('rustc', '--print', 'sysroot').strip()
    version = run('rustc', '--version').strip()
    parser = PlainText()
    parser.feed(Path(sysroot, 'share', 'doc', 'rust', 'COPYRIGHT-library.html').read_text(encoding='utf-8'))
    return f'\n{RULE}\nRust standard library, from {version}\n{RULE}\n\n{parser.text()}'


def main():
    if len(sys.argv) != 2:
        sys.exit('usage: python3 scripts/notices.py OUTPUT')
    sections = [crate_section(lic) for lic in crate_licenses()]
    sections.append(standard_library_section())
    Path(sys.argv[1]).write_text(HEADER + ''.join(sections), encoding='utf-8')


if __name__ == '__main__':
    main()
