import { z } from 'zod';
import { toStructuredError, type StructuredError } from '../lib/errors';

export type PermissionScope = 'read-only' | 'read-write';

export type ToolResult<O = unknown> =
  | { ok: true; data: O }
  | { ok: false; error: StructuredError };

/** A tool is a name + description + Zod input schema + a handler. */
export interface Tool<I = any, O = any> {
  name: string;
  description: string;
  inputSchema: z.ZodType<I>;
  handler(input: I): Promise<O> | O;
}

/**
 * Runs a tool safely: validates input, always returns a structured result
 * ({ok:true,data} | {ok:false,error}). Never throws — the agent gets an
 * actionable error instead of a stack trace (Part I.5).
 */
export async function runTool<I, O>(tool: Tool<I, O>, rawInput: unknown): Promise<ToolResult<O>> {
  const parsed = tool.inputSchema.safeParse(rawInput);
  if (!parsed.success) {
    const issue = parsed.error.issues[0];
    return {
      ok: false,
      error: {
        code: 'INVALID_INPUT',
        reason: issue ? `${issue.path.join('.') || '(root)'}: ${issue.message}` : 'Invalid input.',
        suggestion: `Check the input schema for tool "${tool.name}".`,
      },
    };
  }
  try {
    const data = await tool.handler(parsed.data);
    return { ok: true, data };
  } catch (err) {
    return { ok: false, error: toStructuredError(err) };
  }
}
