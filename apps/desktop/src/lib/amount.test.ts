import {
  formatAmount,
  formatBtc,
  formatSat,
  formatSignedBtc,
  formatSignedSat,
  groupThousands,
  MAX_MONEY_SAT,
  parseAmount,
  parseBtc,
  parseSat,
  satToInput,
  splitBtc,
} from "./amount";

function sat(result: ReturnType<typeof parseBtc>): number {
  if (!result.ok) throw new Error(`expected ok, got: ${result.error}`);
  return result.sat;
}

function error(result: ReturnType<typeof parseBtc>): string {
  if (result.ok) throw new Error(`expected an error, got ${result.sat} sat`);
  return result.error;
}

describe("formatting (matches the CLI's output.rs)", () => {
  it("groups thousands", () => {
    expect(groupThousands(0)).toBe("0");
    expect(groupThousands(999)).toBe("999");
    expect(groupThousands(1_000)).toBe("1,000");
    expect(groupThousands(1_000_000)).toBe("1,000,000");
    expect(groupThousands(12_345_678)).toBe("12,345,678");
    expect(groupThousands(MAX_MONEY_SAT)).toBe("2,100,000,000,000,000");
    expect(groupThousands(-2_141)).toBe("-2,141");
  });

  it("writes BTC with exactly 8 decimals", () => {
    expect(formatBtc(0)).toBe("0.00000000");
    expect(formatBtc(1)).toBe("0.00000001");
    expect(formatBtc(1_000_000)).toBe("0.01000000");
    expect(formatBtc(5_000_000_000)).toBe("50.00000000");
    expect(formatBtc(MAX_MONEY_SAT)).toBe("21000000.00000000");
    expect(formatBtc(MAX_MONEY_SAT - 1)).toBe("20999999.99999999");
    expect(formatBtc(-2_000)).toBe("-0.00002000");
    expect(formatAmount(1_000_000)).toBe("0.01000000 BTC (1,000,000 sat)");
    expect(formatAmount(0)).toBe("0.00000000 BTC (0 sat)");
    expect(formatSat(5_000_002_820)).toBe("5,000,002,820 sat");
  });

  it("signs amounts like the CLI", () => {
    expect(formatSignedBtc(1_000_000)).toBe("+0.01000000");
    expect(formatSignedBtc(-2_141)).toBe("-0.00002141");
    expect(formatSignedBtc(0)).toBe("0.00000000");
    expect(formatSignedSat(-2_141)).toBe("-2,141 sat");
    expect(formatSignedSat(1_000_000)).toBe("+1,000,000 sat");
  });

  it("splits trailing zeros off for display without losing digits", () => {
    expect(splitBtc(1_250_000)).toEqual({ sign: "", significant: "0.0125", zeros: "0000" });
    expect(splitBtc(100_000_000)).toEqual({ sign: "", significant: "1.0", zeros: "0000000" });
    expect(splitBtc(1)).toEqual({ sign: "", significant: "0.00000001", zeros: "" });
    expect(splitBtc(-2_000)).toEqual({ sign: "-", significant: "0.00002", zeros: "000" });
    const { significant, zeros } = splitBtc(123_456_780);
    expect(significant + zeros).toBe(formatBtc(123_456_780));
  });

  it("refuses non-integer satoshis instead of rounding them", () => {
    expect(() => formatBtc(0.5)).toThrow(RangeError);
    expect(() => formatBtc(Number.NaN)).toThrow(RangeError);
    expect(() => groupThousands(2 ** 53)).toThrow(RangeError);
  });
});

describe("parsing BTC without floating point", () => {
  it("avoids the classic float traps", () => {
    // 0.1 + 0.2 !== 0.3 in floats, but in satoshis it is exact.
    expect(sat(parseBtc("0.1")) + sat(parseBtc("0.2"))).toBe(sat(parseBtc("0.3")));
    expect(sat(parseBtc("0.3"))).toBe(30_000_000);
    // 4.35 * 1e8 === 434999999.99999994 and 1.15 * 1e8 === 114999999.99999999 in floats.
    expect(sat(parseBtc("4.35"))).toBe(435_000_000);
    expect(sat(parseBtc("1.15"))).toBe(115_000_000);
    expect(sat(parseBtc("0.29"))).toBe(29_000_000);
    expect(formatBtc(sat(parseBtc("0.3")))).toBe("0.30000000");
  });

  it("accepts up to 8 decimals and nothing smaller than a satoshi", () => {
    expect(sat(parseBtc("0.00000001"))).toBe(1);
    expect(sat(parseBtc("1.00000001"))).toBe(100_000_001);
    expect(sat(parseBtc("0.12345678"))).toBe(12_345_678);
    expect(error(parseBtc("0.000000001"))).toMatch(/8 decimal places/);
    expect(error(parseBtc("1.123456789"))).toMatch(/8 decimal places/);
  });

  it("accepts the usual shorthand", () => {
    expect(sat(parseBtc(" 0.0015 "))).toBe(150_000);
    expect(sat(parseBtc(".5"))).toBe(50_000_000);
    expect(sat(parseBtc("5."))).toBe(500_000_000);
    expect(sat(parseBtc("21"))).toBe(2_100_000_000);
    expect(sat(parseBtc("007.5"))).toBe(750_000_000);
  });

  it("stays within the 21 million BTC supply", () => {
    expect(sat(parseBtc("21000000"))).toBe(MAX_MONEY_SAT);
    expect(sat(parseBtc("20999999.99999999"))).toBe(MAX_MONEY_SAT - 1);
    expect(error(parseBtc("21000000.00000001"))).toMatch(/21 million/);
    expect(error(parseBtc("99999999999999999999"))).toMatch(/21 million/);
  });

  it("rejects anything ambiguous or malformed", () => {
    expect(error(parseBtc(""))).toMatch(/Enter an amount/);
    expect(error(parseBtc("0"))).toMatch(/greater than zero/);
    expect(error(parseBtc("0.00000000"))).toMatch(/greater than zero/);
    expect(error(parseBtc("-1"))).toMatch(/negative/);
    // A decimal comma would otherwise turn 0,001 into 1 BTC.
    expect(error(parseBtc("0,001"))).toMatch(/dot/);
    expect(error(parseBtc("1,000.5"))).toMatch(/dot/);
    expect(error(parseBtc("1e-3"))).toMatch(/number/);
    expect(error(parseBtc("."))).toMatch(/number/);
    expect(error(parseBtc("1.2.3"))).toMatch(/number/);
    expect(error(parseBtc("0x10"))).toMatch(/number/);
    expect(error(parseBtc("½"))).toMatch(/number/);
    expect(error(parseBtc("1 000"))).toMatch(/number/);
  });
});

describe("parsing satoshis", () => {
  it("reads whole numbers with optional grouping", () => {
    expect(sat(parseSat("294"))).toBe(294);
    expect(sat(parseSat("150000"))).toBe(150_000);
    expect(sat(parseSat("150,000"))).toBe(150_000);
    expect(sat(parseSat("1_000_000"))).toBe(1_000_000);
    expect(sat(parseSat("1 000 000"))).toBe(1_000_000);
    expect(sat(parseSat(String(MAX_MONEY_SAT)))).toBe(MAX_MONEY_SAT);
  });

  it("rejects fractions, bad grouping and out-of-range values", () => {
    expect(error(parseSat("1.5"))).toMatch(/whole numbers/);
    expect(error(parseSat("1.000.000"))).toMatch(/whole numbers/);
    expect(error(parseSat("1,00"))).toMatch(/whole number/);
    expect(error(parseSat("12,34,567"))).toMatch(/whole number/);
    expect(error(parseSat("0"))).toMatch(/greater than zero/);
    expect(error(parseSat("-5"))).toMatch(/negative/);
    expect(error(parseSat(String(MAX_MONEY_SAT + 1)))).toMatch(/21 million/);
    expect(error(parseSat("1e5"))).toMatch(/whole number/);
  });

  it("round-trips through the unit toggle", () => {
    for (const value of [1, 294, 150_000, 100_000_000, 123_456_789, MAX_MONEY_SAT]) {
      for (const unit of ["btc", "sat"] as const) {
        expect(sat(parseAmount(satToInput(value, unit), unit))).toBe(value);
      }
    }
    expect(satToInput(150_000, "btc")).toBe("0.0015");
    expect(satToInput(100_000_000, "btc")).toBe("1");
    expect(satToInput(150_000, "sat")).toBe("150000");
  });
});
