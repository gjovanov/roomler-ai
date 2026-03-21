import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

// Mock mediasoup-client
vi.mock('mediasoup-client', () => ({
  Device: vi.fn().mockImplementation(() => ({
    load: vi.fn(),
    rtpCapabilities: {},
    createSendTransport: vi.fn(),
    createRecvTransport: vi.fn(),
  })),
}))

// Mock ws store
vi.mock('@/stores/ws', () => ({
  useWsStore: vi.fn(() => ({
    send: vi.fn(),
    onMediaMessage: vi.fn(),
    offMediaMessage: vi.fn(),
    waitForMessage: vi.fn(),
  })),
}))

import { useConferenceStore } from '@/stores/conference'

describe('useConferenceStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  describe('initial state', () => {
    it('should start not in a call', () => {
      const store = useConferenceStore()
      expect(store.isInCall).toBe(false)
    })

    it('should start with no room info', () => {
      const store = useConferenceStore()
      expect(store.tenantId).toBeNull()
      expect(store.roomId).toBeNull()
      expect(store.roomName).toBeNull()
    })

    it('should start unmuted with video on', () => {
      const store = useConferenceStore()
      expect(store.isMuted).toBe(false)
      expect(store.isVideoOn).toBe(true)
    })

    it('should start not screen sharing', () => {
      const store = useConferenceStore()
      expect(store.isScreenSharing).toBe(false)
    })

    it('should have no local stream', () => {
      const store = useConferenceStore()
      expect(store.localStream).toBeNull()
    })

    it('should have no device', () => {
      const store = useConferenceStore()
      expect(store.device).toBeNull()
    })

    it('should have empty available devices', () => {
      const store = useConferenceStore()
      expect(store.availableDevices).toEqual([])
    })

    it('should have empty device selections', () => {
      const store = useConferenceStore()
      expect(store.selectedAudioDeviceId).toBe('')
      expect(store.selectedVideoDeviceId).toBe('')
    })

    it('should have no remote streams', () => {
      const store = useConferenceStore()
      expect(store.remoteStreams.size).toBe(0)
    })

    it('should have no active speaker', () => {
      const store = useConferenceStore()
      expect(store.activeSpeakerKey).toBeNull()
    })
  })

  describe('enumerateDevices', () => {
    it('should populate available devices', async () => {
      const mockDevices = [
        { deviceId: 'audio1', kind: 'audioinput', label: 'Mic 1' },
        { deviceId: 'video1', kind: 'videoinput', label: 'Camera 1' },
      ] as MediaDeviceInfo[]

      Object.defineProperty(navigator, 'mediaDevices', {
        value: { enumerateDevices: vi.fn().mockResolvedValue(mockDevices) },
        writable: true,
        configurable: true,
      })

      const store = useConferenceStore()
      await store.enumerateDevices()

      expect(store.availableDevices).toEqual(mockDevices)
    })

    it('should handle enumeration errors gracefully', async () => {
      Object.defineProperty(navigator, 'mediaDevices', {
        value: { enumerateDevices: vi.fn().mockRejectedValue(new Error('Permission denied')) },
        writable: true,
        configurable: true,
      })

      const store = useConferenceStore()
      const consoleSpy = vi.spyOn(console, 'error').mockImplementation(() => {})

      await store.enumerateDevices()

      expect(consoleSpy).toHaveBeenCalled()
      expect(store.availableDevices).toEqual([])
      consoleSpy.mockRestore()
    })
  })

  describe('device selection', () => {
    it('should allow setting audio device id', () => {
      const store = useConferenceStore()
      store.selectedAudioDeviceId = 'audio-device-1'
      expect(store.selectedAudioDeviceId).toBe('audio-device-1')
    })

    it('should allow setting video device id', () => {
      const store = useConferenceStore()
      store.selectedVideoDeviceId = 'video-device-1'
      expect(store.selectedVideoDeviceId).toBe('video-device-1')
    })
  })

  describe('toggleMute', () => {
    it('should toggle muted state', () => {
      const store = useConferenceStore()
      expect(store.isMuted).toBe(false)

      store.toggleMute()
      expect(store.isMuted).toBe(true)

      store.toggleMute()
      expect(store.isMuted).toBe(false)
    })
  })

  describe('toggleVideo', () => {
    it('should toggle video state', () => {
      const store = useConferenceStore()
      expect(store.isVideoOn).toBe(true)

      store.toggleVideo()
      expect(store.isVideoOn).toBe(false)

      store.toggleVideo()
      expect(store.isVideoOn).toBe(true)
    })
  })

  describe('leaveRoom', () => {
    it('should reset all state', () => {
      const store = useConferenceStore()

      // Simulate being in a call
      store.isInCall = true
      store.isMuted = true
      store.isVideoOn = false
      store.isScreenSharing = true
      store.tenantId = 't1'
      store.roomId = 'r1'
      store.roomName = 'Test Room'

      store.leaveRoom()

      expect(store.isInCall).toBe(false)
      expect(store.isMuted).toBe(false)
      expect(store.isVideoOn).toBe(true)
      expect(store.isScreenSharing).toBe(false)
      expect(store.tenantId).toBeNull()
      expect(store.roomId).toBeNull()
      expect(store.roomName).toBeNull()
      expect(store.device).toBeNull()
      expect(store.sendTransport).toBeNull()
      expect(store.recvTransport).toBeNull()
      expect(store.localStream).toBeNull()
      expect(store.remoteStreams.size).toBe(0)
    })
  })

  describe('screen sharing state', () => {
    it('should track screen sharing state via isScreenSharing', () => {
      const store = useConferenceStore()
      expect(store.isScreenSharing).toBe(false)
      store.isScreenSharing = true
      expect(store.isScreenSharing).toBe(true)
    })
  })

  describe('producers and consumers', () => {
    it('should start with empty producers', () => {
      const store = useConferenceStore()
      expect(store.producers.size).toBe(0)
    })

    it('should start with empty consumers', () => {
      const store = useConferenceStore()
      expect(store.consumers.size).toBe(0)
    })
  })
})
