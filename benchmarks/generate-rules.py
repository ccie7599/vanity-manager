#!/usr/bin/env python3
"""Generate synthetic redirect rules for scale testing.

Usage:
    python3 benchmarks/generate-rules.py --count 10000 --hosts 5 > rules-10k.json
    python3 benchmarks/generate-rules.py --count 12000 --hosts 12000 --shape wide > rules-wide.json

Two distributions:
    tall — few hosts × many rules each (mass path/marketing redirects)
    wide — many hosts × one rule each (vanity / portfolio-style)
"""
from __future__ import annotations
import argparse
import json
import random
import sys

WORDS = [
    "mortgage", "checking", "savings", "loans", "credit", "deposit", "invest",
    "retire", "planning", "advisor", "lending", "calculator", "rates", "offer",
    "promo", "apply", "open", "locator", "branch", "find", "about", "contact",
    "support", "help", "faq", "terms", "privacy", "locations", "products",
    "services", "business", "commercial", "personal", "wealth", "mobile",
    "online", "banking", "auto", "home", "student", "refi", "hero", "deal",
]


def gen_path(depth: int) -> str:
    return "/" + "/".join(random.choices(WORDS, k=depth))


def tall_shape(count: int, hosts: int) -> list[dict]:
    rules = []
    host_list = [f"host{i}.vanity-test.example.com" for i in range(hosts)]
    per_host = count // max(hosts, 1)
    remainder = count % max(hosts, 1)
    for h_idx, host in enumerate(host_list):
        n = per_host + (1 if h_idx < remainder else 0)
        seen: set[str] = set()
        for _ in range(n):
            for _attempt in range(10):
                depth = random.choice([1, 1, 1, 2, 2, 3])
                path = gen_path(depth)
                if path not in seen:
                    seen.add(path)
                    break
            else:
                continue
            match_type = random.choices(["exact", "prefix"], weights=[85, 15])[0]
            rules.append({
                "host": host,
                "path": path,
                "target_url": f"https://new-{h_idx}.vanity-test.example.com{gen_path(random.choice([1,2]))}",
                "status_code": random.choices([301, 302], weights=[80, 20])[0],
                "match_type": match_type,
                "preserve_path": match_type == "prefix" and random.random() < 0.5,
                "preserve_query": True,
                "enabled": True,
                "priority": 0,
                "notes": f"synth-{h_idx}",
            })
    return rules


def wide_shape(count: int) -> list[dict]:
    # One rule per host, apex redirect to a property code
    rules = []
    for i in range(count):
        host = f"property-{i:05d}.vanity-test.example.com"
        code = f"HOT{i:04x}"
        rules.append({
            "host": host,
            "path": "/",
            "target_url": f"https://www.brand.example.com/hotels/travel/{code}",
            "status_code": 301,
            "match_type": "exact",
            "preserve_path": False,
            "preserve_query": True,
            "enabled": True,
            "priority": 0,
            "notes": f"vanity-{code}",
        })
    return rules


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--count", type=int, default=1000)
    p.add_argument("--hosts", type=int, default=10)
    p.add_argument("--shape", choices=["tall", "wide"], default="tall")
    p.add_argument("--seed", type=int, default=42)
    args = p.parse_args()
    random.seed(args.seed)

    if args.shape == "wide":
        rules = wide_shape(args.count)
    else:
        rules = tall_shape(args.count, args.hosts)

    json.dump(rules, sys.stdout, indent=2)
    sys.stdout.write("\n")
    print(
        f"\ngenerated {len(rules)} rules ({args.shape}) across "
        f"{len({r['host'] for r in rules})} hosts",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
