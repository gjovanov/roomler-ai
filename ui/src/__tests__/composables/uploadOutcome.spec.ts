// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #2930 — on a host whose agent runs as a root service, a drawer upload to a
// folder outside the active user's home (`/tmp/x`) landed in ~/Downloads, and
// the viewer said only "Uploaded <name>". The agent refuses such a `dest_path`
// and falls back to Downloads, saying so only in its own log; the viewer
// learns it from the completed file's `path`.
import { describe, it, expect } from 'vitest'
import { uploadLandedIn, uploadOutcomeMessage, type UploadOutcome } from '@/composables/useRemoteControl'

const done = (name: string, path: string): UploadOutcome => ({ ok: true, name, path, bytes: 42 })
const fail = (name: string, error: string): UploadOutcome => ({ ok: false, name, error })

describe('uploadLandedIn (#2930)', () => {
  it('a file in the folder asked for, or below it, landed in it', () => {
    expect(uploadLandedIn('/tmp/x/f.txt', '/tmp/x')).toBe(true)
    expect(uploadLandedIn('/tmp/x/a/b/f.txt', '/tmp/x/')).toBe(true)
    expect(uploadLandedIn('/f.txt', '/')).toBe(true)
  })

  it('the field case: the agent fell back to Downloads', () => {
    expect(uploadLandedIn('/home/u/Downloads/f.txt', '/tmp/x')).toBe(false)
  })

  it('compares whole segments: /tmp/x does not hold /tmp/xy', () => {
    expect(uploadLandedIn('/tmp/xy/f.txt', '/tmp/x')).toBe(false)
  })

  it('POSIX names are case-sensitive', () => {
    expect(uploadLandedIn('/tmp/X/f.txt', '/tmp/x')).toBe(false)
  })

  it('Windows paths: separators, case and the verbatim prefix do not matter', () => {
    expect(uploadLandedIn('C:\\Users\\A\\Desktop\\f.txt', 'c:/users/a/desktop')).toBe(true)
    expect(uploadLandedIn('\\\\?\\C:\\Users\\A\\Desktop\\f.txt', 'C:\\Users\\A\\Desktop\\')).toBe(true)
    expect(uploadLandedIn('C:\\Users\\A\\Downloads\\f.txt', 'C:\\Users\\A\\Desktop')).toBe(false)
  })
})

describe('uploadOutcomeMessage (#2930)', () => {
  it('the field case: one file the host put in Downloads is a warning that says where', () => {
    expect(uploadOutcomeMessage([done('f.txt', '/home/u/Downloads/f.txt')], 1, '/tmp/x')).toEqual({
      level: 'warning',
      text: 'Uploaded f.txt to /home/u/Downloads instead: the host refused /tmp/x',
    })
  })

  it('a folder upload names the folder its whole tree landed in', () => {
    const results = [
      done('Pics/a/1.png', '/home/u/Downloads/Pics/a/1.png'),
      done('Pics/2.png', '/home/u/Downloads/Pics/2.png'),
    ]
    expect(uploadOutcomeMessage(results, 2, '/tmp/x').text).toBe(
      'Uploaded 2 files to /home/u/Downloads instead: the host refused /tmp/x',
    )
  })

  it('Windows: the folder reads without the verbatim prefix', () => {
    expect(uploadOutcomeMessage([done('f.txt', '\\\\?\\C:\\Users\\A\\Downloads\\f.txt')], 1, 'C:\\Temp').text).toBe(
      'Uploaded f.txt to C:\\Users\\A\\Downloads instead: the host refused C:\\Temp',
    )
  })

  it('only some landed elsewhere: it says how many', () => {
    const results = [done('a', '/tmp/x/a'), done('b', '/home/u/Downloads/b')]
    expect(uploadOutcomeMessage(results, 2, '/tmp/x').text).toBe(
      'Uploaded 2 files, 1 of them to /home/u/Downloads instead: the host refused /tmp/x',
    )
  })

  it('a file in the folder asked for is a plain success, as before', () => {
    expect(uploadOutcomeMessage([done('f.txt', '/tmp/x/f.txt')], 1, '/tmp/x')).toEqual({
      level: 'success',
      text: 'Uploaded f.txt',
    })
    expect(uploadOutcomeMessage([done('a', '/tmp/x/a'), done('b', '/tmp/x/b')], 2, '/tmp/x')).toEqual({
      level: 'success',
      text: 'Uploaded 2 files',
    })
  })

  it('with no folder asked for (the default, Downloads), wherever it landed is a success', () => {
    expect(uploadOutcomeMessage([done('f.txt', '/home/u/Downloads/f.txt')], 1)).toEqual({
      level: 'success',
      text: 'Uploaded f.txt',
    })
  })

  it('failures keep the messages they had', () => {
    expect(uploadOutcomeMessage([fail('f.txt', 'boom')], 1, '/tmp/x')).toEqual({
      level: 'error',
      text: 'Upload failed: boom',
    })
    expect(uploadOutcomeMessage([fail('a', 'boom'), fail('b', 'bang')], 2)).toEqual({
      level: 'error',
      text: '2 uploads failed (first: a — boom)',
    })
    expect(uploadOutcomeMessage([done('a', '/tmp/x/a'), fail('b', 'bang')], 2, '/tmp/x')).toEqual({
      level: 'error',
      text: 'Uploaded 1/2 — 1 failed (e.g. b: bang)',
    })
  })
})
