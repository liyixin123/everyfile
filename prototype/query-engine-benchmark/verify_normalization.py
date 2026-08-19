#!/usr/bin/env python3
import unicodedata

cases = {
    "Résumé.PDF": "résumé.pdf",
    "RE\u0301SUME\u0301.PDF": "résumé.pdf",
    "Straße.txt": "strasse.txt",
    "STRASSE.TXT": "strasse.txt",
    "Ångström.swift": "ångström.swift",
    "A\u030angstro\u0308m.SWIFT": "ångström.swift",
}
for source, expected in cases.items():
    actual = unicodedata.normalize("NFC", source).casefold()
    assert actual == expected, (source, actual, expected)
print(f"normalization_ok,{len(cases)}")
