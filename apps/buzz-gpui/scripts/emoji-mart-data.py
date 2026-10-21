#!/usr/bin/env python3
"""Writes the emoji picker's search data from emoji-mart's dataset.

Usage:
  emoji-mart-data.py <buzz-app>/node_modules/@emoji-mart/data/sets/15/native.json \
    > crates/app/src/ui/emoji_picker/emoji-mart.tsv

Each line is `id <TAB> native <TAB> skins <TAB> terms <TAB> aliases`. The
terms are emoji-mart's search string after the id (name words, keywords,
emoticons; lowercased, in order), because Mart scores a match by its
position in that string.
"""
import json
import re
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
aliases = {}
for alias, target in data["aliases"].items():
    aliases.setdefault(target, []).append(alias)
lines = []
for emoji in data["emojis"].values():
    assert emoji["id"] == emoji["id"].lower() and "," not in emoji["id"]
    terms = [s.lower() for s in re.split(r"[-|_\s]+", emoji["name"])]
    terms += [k.lower() for k in emoji.get("keywords", [])]
    terms += [k.lower() for k in emoji.get("emoticons", [])]
    terms = [t for t in terms if t and t.strip()]
    assert not any("\t" in t or "\n" in t for t in terms)
    lines.append(
        "\t".join(
            [
                emoji["id"],
                emoji["skins"][0]["native"],
                str(len(emoji["skins"])),
                ",".join(terms),
                ",".join(aliases.get(emoji["id"], [])),
            ]
        )
    )
sys.stdout.write("\n".join(lines) + "\n")
