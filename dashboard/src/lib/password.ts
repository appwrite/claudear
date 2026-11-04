export const PASSWORD_MIN_BYTES = 8
export const PASSWORD_MAX_BYTES = 72

export const PASSWORD_LENGTH_MESSAGE = `Password must be ${PASSWORD_MIN_BYTES}-${PASSWORD_MAX_BYTES} bytes`

const encoder = new TextEncoder()

/** Bcrypt only reads the first 72 bytes, so the server bounds passwords in UTF-8 bytes, not characters. */
export function passwordByteLength(password: string): number {
  return encoder.encode(password).length
}

export function passwordLengthProblem(password: string): string | null {
  const length = passwordByteLength(password)
  return length < PASSWORD_MIN_BYTES || length > PASSWORD_MAX_BYTES ? PASSWORD_LENGTH_MESSAGE : null
}
