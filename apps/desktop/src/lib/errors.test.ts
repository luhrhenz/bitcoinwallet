import { describeError, errorText, isApiError, toApiError } from "./errors";

describe("describeError", () => {
  it("gives every core code a plain-language title", () => {
    const codes = [
      "invalid_mnemonic",
      "wrong_password",
      "weak_password",
      "wallet_exists",
      "wallet_not_found",
      "wallet_in_use",
      "network_mismatch",
      "mainnet_disabled",
      "invalid_address",
      "dust_amount",
      "insufficient_funds",
      "tx_build",
      "sign",
      "tx_not_found",
      "contact",
      "rpc",
      "persist",
      "keystore",
      "config",
      "io",
      "locked",
    ];
    for (const code of codes) {
      const { title } = describeError({ code, message: "x" });
      expect(title, code).not.toBe("Something went wrong.");
      if (code.includes("_")) expect(title, code).not.toContain(code);
    }
  });

  it("says what to do for the common failures", () => {
    expect(describeError({ code: "wrong_password", message: "wrong password or corrupted keystore" })).toEqual({
      code: "wrong_password",
      title: "That password is not correct. Try again.",
      detail: null,
    });
    expect(describeError({ code: "weak_password", message: "password must be at least 8 characters" }).title).toBe(
      "Choose a password with at least 8 characters.",
    );
    expect(describeError({ code: "wallet_in_use", message: "" }).title).toMatch(/another btcw window or terminal/);
    expect(describeError({ code: "wallet_not_found", message: "" }).title).toMatch(/Create one or restore one/);
    expect(describeError({ code: "dust_amount", message: "" }).title).toMatch(/minimum is 294 sat/);
  });

  it("keeps the useful part of the core's message as detail", () => {
    expect(
      describeError({ code: "rpc", message: "bitcoin node RPC error: Connection refused (os error 111)" }),
    ).toEqual({
      code: "rpc",
      title: "Can't reach your Bitcoin node. Check that bitcoind is running and that the node settings are right.",
      detail: "Connection refused (os error 111)",
    });
    expect(
      errorText({ code: "insufficient_funds", message: "insufficient funds: need 0.002 BTC, available 0.001 BTC" }),
    ).toBe("Not enough funds to send this amount plus the network fee. (need 0.002 BTC, available 0.001 BTC)");
    expect(
      describeError({ code: "invalid_mnemonic", message: "invalid mnemonic: word 5 is not in the BIP39 English word list" })
        .detail,
    ).toBe("word 5 is not in the BIP39 English word list");
  });

  it("explains address book and label refusals", () => {
    expect(
      describeError({ code: "contact", message: "no contact is named `Alcie`, and it is not a valid address either" })
        .title,
    ).toMatch(/^No contact has that name, and it isn't a valid address either/);
    expect(describeError({ code: "contact", message: "no contact is named `Bob`" }).title).toMatch(
      /^There's no contact with that name/,
    );
    expect(describeError({ code: "contact", message: "a contact named `Alice` already exists" })).toEqual({
      code: "contact",
      title: "That name, note or label can't be used.",
      detail: "A contact named `Alice` already exists",
    });
  });

  it("names the right address prefix for the network", () => {
    const error = { code: "network_mismatch", message: "network mismatch: expected regtest, found a testnet4/signet address" };
    expect(describeError(error, { network: "regtest" }).title).toBe(
      "That address is for a different network. On regtest, addresses start with bcrt1.",
    );
    expect(describeError(error, { network: "testnet4" }).title).toMatch(/start with tb1\.$/);
  });

  it("never shows a stack trace or raw object", () => {
    const err = new Error("kaboom");
    err.stack = "Error: kaboom\n    at main.rs:42";
    const described = describeError(err);
    expect(described).toEqual({ code: "unknown", title: "Something went wrong.", detail: "kaboom" });
    expect(describeError({ code: "x", message: "line one\n    at frame" }).detail).toBe("line one");
    expect(describeError(42).detail).toBeNull();
    expect(describeError("command app_info not found").detail).toBe("command app_info not found");
    expect(describeError({ code: "x", message: "y".repeat(500) }).detail?.length).toBe(240);
  });

  it("recognises ApiError shapes", () => {
    expect(isApiError({ code: "rpc", message: "m" })).toBe(true);
    expect(isApiError({ code: "rpc" })).toBe(false);
    expect(isApiError(null)).toBe(false);
    expect(toApiError("plain")).toEqual({ code: "unknown", message: "plain" });
  });
});
