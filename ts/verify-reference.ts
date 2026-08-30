#!/usr/bin/env bun
/**
 * Run the conformance vectors against the TypeScript reference implementation.
 *
 * This reads the reference repo and writes nothing to it. Its purpose is to establish, before
 * any Rust is written, exactly where the reference agrees with the intended behavior and where
 * it does not — so the Rust implementation is measured against a known baseline rather than
 * against a guess about one.
 *
 * A vector marked `reference_agrees: false` is EXPECTED to fail here. That is the finding, not
 * a broken test: the reference's own stated invariant is that no IEEE-754 value touches a token
 * amount, and these are the cases where one does.
 *
 *   bun ts/verify-reference.ts [path-to-reference-repo]
 */

import { readFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REFERENCE =
  process.argv[2] ?? "/Users/rifuki/mgodonf/web3/sui/deepsurge/rill";

const fixtures = JSON.parse(
  readFileSync(resolve(HERE, "../fixtures/amounts.json"), "utf8"),
);

const { decimalToBaseUnits, parseU64String } = await import(
  resolve(REFERENCE, "packages/rill-sdk/src/amounts.ts")
);

/** The reference's price conversion, reproduced verbatim from @mysten/deepbook-v3. */
const referenceConvertPrice = (
  value: number,
  floatScalar: number,
  quoteScalar: number,
  baseScalar: number,
) => BigInt(Math.round((value * floatScalar * quoteScalar) / baseScalar));

/** The reference's quantity conversion, likewise verbatim. */
const referenceConvertQuantity = (value: number, scalar: number) =>
  BigInt(Math.round(value * scalar));

let pass = 0;
let expectedFail = 0;
let surprise = 0;

const ok = (m: string) => { pass++; console.log(`  ok        ${m}`); };
const known = (m: string) => { expectedFail++; console.log(`  KNOWN GAP ${m}`); };
const bad = (m: string) => { surprise++; console.log(`  UNEXPECTED ${m}`); };

console.log("\ndecimal_to_base_units — accepted");
for (const v of fixtures.decimal_to_base_units.accepted) {
  try {
    const got = decimalToBaseUnits(v.value, v.decimals).toString();
    got === v.expected
      ? ok(`"${v.value}" @${v.decimals} -> ${got}`)
      : bad(`"${v.value}" @${v.decimals} -> ${got}, expected ${v.expected}`);
  } catch (e) {
    bad(`"${v.value}" @${v.decimals} threw: ${(e as Error).message}`);
  }
}

console.log("\ndecimal_to_base_units — rejected");
for (const v of fixtures.decimal_to_base_units.rejected) {
  try {
    const got = decimalToBaseUnits(v.value, v.decimals);
    bad(`"${v.value}" @${v.decimals} was accepted as ${got} — expected rejection (${v.why})`);
  } catch {
    ok(`"${v.value}" rejected (${v.why})`);
  }
}

console.log("\nparse_u64 — accepted");
for (const v of fixtures.parse_u64.accepted) {
  try {
    const got = parseU64String(v.value, "field").toString();
    got === v.expected ? ok(`"${v.value}"`) : bad(`"${v.value}" -> ${got}, expected ${v.expected}`);
  } catch (e) {
    bad(`"${v.value}" threw: ${(e as Error).message}`);
  }
}

console.log("\nparse_u64 — rejected");
for (const v of fixtures.parse_u64.rejected) {
  try {
    parseU64String(v.value, "field");
    bad(`"${v.value}" was accepted — expected rejection (${v.why})`);
  } catch {
    ok(`"${v.value}" rejected (${v.why})`);
  }
}

console.log("\ndeepbook_price — the money path");
const FLOAT = Number(fixtures.deepbook_price.$float_scalar);
for (const v of fixtures.deepbook_price.vectors) {
  const got = referenceConvertPrice(
    Number(v.price),
    FLOAT,
    Number(v.quote_scalar),
    Number(v.base_scalar),
  ).toString();
  const agrees = got === v.expected;
  const label = `${v.price} on ${v.pool_shape} -> ${got}`;
  if (agrees && v.reference_agrees !== false) ok(label);
  else if (!agrees && v.reference_agrees === false)
    known(`${label}, exact is ${v.expected} (off by ${BigInt(got) - BigInt(v.expected)})`);
  else if (!agrees) bad(`${label}, expected ${v.expected}`);
  else bad(`${label} — agreed, but the vector says it should not have`);
}

console.log("\ndeepbook_quantity");
for (const v of fixtures.deepbook_quantity.vectors) {
  const got = referenceConvertQuantity(Number(v.quantity), Number(v.base_scalar)).toString();
  const agrees = got === v.expected;
  if (agrees && v.reference_agrees !== false) ok(`${v.quantity} -> ${got}`);
  else if (!agrees && v.reference_agrees === false)
    known(`${v.quantity} -> ${got}, exact is ${v.expected} (off by ${BigInt(got) - BigInt(v.expected)})`);
  else if (!agrees) bad(`${v.quantity} -> ${got}, expected ${v.expected}`);
  else bad(`${v.quantity} -> ${got} — agreed, but the vector says it should not have`);
}

console.log(
  `\n${pass} agreed · ${expectedFail} known gap(s) · ${surprise} unexpected\n`,
);
if (surprise > 0) {
  console.log("An UNEXPECTED result means the reference behaves differently from what the");
  console.log("vectors assume. Investigate before writing Rust against them.\n");
  process.exit(1);
}
