/**
 * Every Tacitus tool failure is structured — `{ code, reason, suggestion }`,
 * never a bare "failed". The SDK surfaces them as thrown errors so callers
 * can branch on `code` programmatically (`CONFLICT` → re-list and retry,
 * `MISSING_VARS` → read the reason for what to add, …).
 */
export class TacitusToolError extends Error {
  constructor(
    /** Machine-matchable code, e.g. "NOTE_NOT_FOUND", "PERMISSION_DENIED". */
    public readonly code: string,
    /** What went wrong. */
    public readonly reason: string,
    /** What to do differently. */
    public readonly suggestion: string,
  ) {
    super(`${code}: ${reason} ${suggestion}`);
    this.name = 'TacitusToolError';
  }
}
