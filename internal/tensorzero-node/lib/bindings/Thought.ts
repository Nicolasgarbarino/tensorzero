// This file was generated by [ts-rs](https://github.com/Aleph-Alpha/ts-rs). Do not edit this file manually.

/**
 * Struct that represents Chain of Thought reasoning
 */
export type Thought = {
  text: string | null;
  /**
   * An optional signature - currently, this is only used with Anthropic,
   * and is ignored by other providers.
   */
  signature: string | null;
};
