import { describe, expect, it } from 'vitest'
import {
  MEDIA_SILENT_TICKS,
  agentSaysMediaPlan,
  describeMediaPromise,
  describeMediaStep,
  describeMediaStop,
  mediaLadderLabel,
  mediaPlanAction,
  nextMediaSilent,
  parseMediaPlan,
  viewerHzWireMessage,
  type RcMediaPlan,
} from '@/composables/mediaPlan'
import { parseControlInbound } from '@/composables/useRemoteControl'

const step = (from: string, to: string, why = 'because') => ({ from, to, why, at_ms: 1 })

describe('FR-135 R2: rc:media-plan on the wire', () => {
  it('reads the agent\'s first message, which has only what it knows', () => {
    const plan = parseMediaPlan({ t: 'rc:media-plan', epoch: 1, transport: 'data-channel-h264', codec: 'h264', ladder: [] })
    expect(plan).toEqual({
      epoch: 1,
      transport: 'data-channel-h264',
      codec: 'h264',
      chroma: null,
      cell: null,
      input: null,
      fps: null,
      capture: null,
      ladder: [],
      stopped: null,
    })
  })

  it('reads a full plan', () => {
    const plan = parseMediaPlan({
      t: 'rc:media-plan',
      epoch: 7,
      transport: 'data-channel-hevc',
      codec: 'h265',
      chroma: 'yuv444',
      cell: 'hevc_nvenc:yuv444',
      input: { form: 'cpu', reason: 'no_gpu_cell' },
      fps: { asked: 144, granted: 120, reason: 'refresh' },
      capture: { arm: 'wgc', form: 'gpu' },
      ladder: [{ from: 'hevc_qsv', to: 'hevc_nvenc', why: 'no adapter', at_ms: 12 }],
      stopped: { why: 'encoder_fault', detail: 'gone' },
    })
    expect(plan?.cell).toBe('hevc_nvenc:yuv444')
    expect(plan?.input).toEqual({ form: 'cpu', reason: 'no_gpu_cell' })
    expect(plan?.fps).toEqual({ asked: 144, granted: 120, reason: 'refresh' })
    expect(plan?.capture).toEqual({ arm: 'wgc', form: 'gpu' })
    expect(plan?.ladder).toEqual([{ from: 'hevc_qsv', to: 'hevc_nvenc', why: 'no adapter', at_ms: 12 }])
    expect(plan?.stopped).toEqual({ why: 'encoder_fault', detail: 'gone' })
  })

  it('reads a granted rate with no ask, the common case', () => {
    expect(parseMediaPlan({ epoch: 1, transport: 'webrtc', codec: 'h264', fps: { granted: 60 } })?.fps).toEqual({
      asked: null,
      granted: 60,
      reason: null,
    })
  })

  it('is no plan without its epoch, transport and codec', () => {
    expect(parseMediaPlan({ transport: 'webrtc', codec: 'h264' })).toBeNull()
    expect(parseMediaPlan({ epoch: 'one', transport: 'webrtc', codec: 'h264' })).toBeNull()
    expect(parseMediaPlan({ epoch: 1, codec: 'h264' })).toBeNull()
    expect(parseMediaPlan({ epoch: 1, transport: 'webrtc' })).toBeNull()
  })

  it('skips what a newer or broken agent sends, instead of failing the plan', () => {
    const plan = parseMediaPlan({
      epoch: 2,
      transport: 'webrtc',
      codec: 'h264',
      a_field_from_the_future: { x: 1 },
      input: 'cpu',
      fps: { asked: 144 },
      capture: [],
      ladder: [step('a', 'b'), 'not a step', { from: 'c' }, null],
      stopped: { detail: 'no word for it' },
    })
    expect(plan?.input).toBeNull()
    expect(plan?.fps).toBeNull()
    expect(plan?.capture).toBeNull()
    expect(plan?.ladder).toHaveLength(1)
    expect(plan?.stopped).toBeNull()
  })

  it('reaches the control parser, and a broken one is dropped like any unknown message', () => {
    const parsed = parseControlInbound(JSON.stringify({ t: 'rc:media-plan', epoch: 1, transport: 'webrtc', codec: 'h264', ladder: [] }))
    expect(parsed?.kind).toBe('media_plan')
    expect(parseControlInbound(JSON.stringify({ t: 'rc:media-plan' }))).toBeNull()
  })

  it('matches the verb by equality', () => {
    expect(agentSaysMediaPlan(['exec', 'media-plan'])).toBe(true)
    expect(agentSaysMediaPlan(['media-plan-v2', 'media'])).toBe(false)
    expect(agentSaysMediaPlan([])).toBe(false)
    expect(agentSaysMediaPlan(undefined)).toBe(false)
    expect(agentSaysMediaPlan(null)).toBe(false)
  })

  it('tells the agent the rate in the shape it parses', () => {
    expect(viewerHzWireMessage(144)).toEqual({ t: 'rc:viewer-hz', hz: 144 })
  })
})

describe('FR-135 R2: what the page says', () => {
  it('names every cell it tried, once, and the device\'s own words', () => {
    const copy = describeMediaStop({
      stopped: { why: 'no_encoder', detail: 'no vulkan device opened on this host' },
      ladder: [step('h264_nvenc', 'h264_amf'), step('h264_amf', 'h264_vulkan'), step('h264_vulkan', 'none')],
    })
    expect(copy?.title).toBe('This device could not start its video.')
    expect(copy?.body).toBe('It tried h264_nvenc, h264_amf and h264_vulkan, and none of them opened.')
    expect(copy?.code).toBe('no_encoder')
    expect(copy?.detail).toBe('no vulkan device opened on this host')
  })

  it('counts cells, not the other arrows on the ladder', () => {
    const copy = describeMediaStop({
      stopped: { why: 'no_encoder', detail: '' },
      ladder: [step('144 fps', '60 fps'), step('gpu', 'cpu'), step('h264_qsv', 'none')],
    })
    expect(copy?.body).toBe('It tried h264_qsv, and none of them opened.')
  })

  it('says something when nothing was tried', () => {
    expect(describeMediaStop({ stopped: { why: 'no_encoder', detail: '' }, ladder: [] })?.body).toBe(
      'It has no encoder it can open for this stream.',
    )
  })

  it('names the cell that failed in a running session', () => {
    const copy = describeMediaStop({ stopped: { why: 'encoder_fault', detail: 'gone' }, ladder: [step('h264_qsv:yuv420', 'none')] })
    expect(copy?.title).toBe("This device's video encoder stopped.")
    expect(copy?.body).toBe('h264_qsv:yuv420 failed and would not restart.')
  })

  it('keeps a word it does not know, because that still says something', () => {
    const copy = describeMediaStop({ stopped: { why: 'a_new_reason', detail: 'x' }, ladder: [] })
    expect(copy?.title).toBe('This device stopped its video.')
    expect(copy?.code).toBe('a_new_reason')
    expect(copy?.detail).toBe('x')
  })

  it('has no words for a plan that did not stop', () => {
    expect(describeMediaStop({ stopped: null, ladder: [step('a', 'b')] })).toBeNull()
  })

  it('says what was promised', () => {
    expect(describeMediaPromise({ codec: 'h265', transport: 'data-channel-hevc' })).toBe('H.265 over the data channel')
    expect(describeMediaPromise({ codec: 'av1', transport: 'webrtc' })).toBe('AV1 over WebRTC')
    expect(describeMediaPromise({ codec: 'h264', transport: 'data-channel-h264' })).toBe('H.264 over the data channel')
    expect(describeMediaPromise({ codec: 'vp9', transport: 'data-channel-vp9-444' })).toBe('VP9 over the data channel')
    expect(describeMediaPromise({ codec: 'vvc', transport: 'webrtc' })).toBe('vvc over WebRTC')
  })

  it('shows the last arrow, and how many there were', () => {
    expect(mediaLadderLabel([])).toBe('')
    expect(mediaLadderLabel([step('gpu', 'cpu')])).toBe('gpu → cpu')
    expect(mediaLadderLabel([step('a', 'b'), step('b', 'c')])).toBe('b → c (2 steps)')
    expect(describeMediaStep(step('gpu', 'cpu', 'cpu_frames'))).toBe('gpu → cpu: cpu_frames')
    expect(describeMediaStep(step('gpu', 'cpu', ''))).toBe('gpu → cpu')
  })
})

describe('FR-135 R2: what a page does with a plan', () => {
  const planWith = (over: Partial<RcMediaPlan>): RcMediaPlan => ({
    epoch: 2,
    transport: 'data-channel-h264',
    codec: 'h264',
    chroma: null,
    cell: null,
    input: null,
    fps: null,
    capture: null,
    ladder: [],
    stopped: null,
    ...over,
  })

  it('drops an older epoch and takes a newer or the same one', () => {
    expect(mediaPlanAction(planWith({ epoch: 2 }), 3, true)).toBeNull()
    expect(mediaPlanAction(planWith({ epoch: 3 }), 3, true)).not.toBeNull()
    expect(mediaPlanAction(planWith({ epoch: 4 }), 3, true)).not.toBeNull()
  })

  it('takes a re-created session\'s first plan: its epoch starts over and the page\'s newest is reset', () => {
    expect(mediaPlanAction(planWith({ epoch: 1 }), 0, true)).not.toBeNull()
  })

  it('words a stop', () => {
    const stop = mediaPlanAction(planWith({ stopped: { why: 'no_encoder', detail: 'x' } }), 0, true)?.stop
    expect(stop?.code).toBe('no_encoder')
    expect(stop?.detail).toBe('x')
  })

  it('clears the words when an encoder opened', () => {
    expect(mediaPlanAction(planWith({ cell: 'h264_qsv:yuv420' }), 0, true)?.stop).toBeNull()
  })

  it('leaves the words alone on a plan that knows neither: they would blink at every re-create', () => {
    expect(mediaPlanAction(planWith({}), 0, true)?.stop).toBeUndefined()
  })

  it('shows the track when the device is on it and this page decodes a data channel', () => {
    expect(mediaPlanAction(planWith({ transport: 'webrtc' }), 0, true)?.showTrack).toBe(true)
  })

  it('has nothing to show on a page already on the track, or a device on the channel', () => {
    expect(mediaPlanAction(planWith({ transport: 'webrtc' }), 0, false)?.showTrack).toBe(false)
    expect(mediaPlanAction(planWith({ transport: 'data-channel-h264' }), 0, true)?.showTrack).toBe(false)
  })
})

describe('FR-135 R2: when a page with a plan says it has no picture', () => {
  const silent = (over: { prev?: boolean; advanced?: boolean; painted?: boolean; plan?: boolean; ticks?: number }) =>
    nextMediaSilent(over.prev ?? false, over.advanced ?? false, over.painted ?? false, over.plan ?? true, over.ticks ?? MEDIA_SILENT_TICKS)

  it('waits for the watchdog\'s probe', () => {
    expect(silent({ ticks: MEDIA_SILENT_TICKS - 1 })).toBe(false)
    expect(silent({ ticks: MEDIA_SILENT_TICKS })).toBe(true)
  })

  it('needs a plan: an older agent\'s session reads as it did', () => {
    expect(silent({ plan: false })).toBe(false)
  })

  it('leaves a session that painted and then stalled to the watchdog', () => {
    expect(silent({ painted: true })).toBe(false)
  })

  it('stays until the first picture, across the watchdog\'s re-create', () => {
    expect(silent({ prev: true, ticks: 0 })).toBe(true)
    expect(silent({ prev: true, plan: false, ticks: 0 })).toBe(true)
    expect(silent({ prev: true, advanced: true })).toBe(false)
  })
})
