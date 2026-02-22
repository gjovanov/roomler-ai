import { ref } from 'vue'
import { useWsStore } from '@/stores/ws'

interface PlaybackMessage {
  action: 'start' | 'stop'
  file_url?: string
  file_id?: string
  filename?: string
  playback_id: string
  room_id: string
}

export function useAudioPlayback() {
  const wsStore = useWsStore()
  const activePlayback = ref<{ id: string; audio: HTMLAudioElement } | null>(null)
  const isPlaying = ref(false)

  function handlePlaybackMessage(data: PlaybackMessage) {
    if (data.action === 'start' && data.file_url) {
      // Stop any existing playback first
      stopCurrentPlayback()

      const audio = new Audio(data.file_url)
      audio.addEventListener('ended', () => {
        isPlaying.value = false
        activePlayback.value = null
      })
      audio.addEventListener('error', (e) => {
        console.error('Audio playback error:', e)
        isPlaying.value = false
        activePlayback.value = null
      })
      audio.play().catch((err) => {
        console.error('Failed to start audio playback:', err)
      })

      activePlayback.value = { id: data.playback_id, audio }
      isPlaying.value = true
    } else if (data.action === 'stop') {
      if (activePlayback.value?.id === data.playback_id) {
        stopCurrentPlayback()
      }
    }
  }

  function requestPlay(roomId: string, fileId: string) {
    wsStore.send('media:play_audio', {
      room_id: roomId,
      file_id: fileId,
    })
  }

  function requestStop(roomId: string, playbackId: string) {
    wsStore.send('media:stop_audio', {
      room_id: roomId,
      playback_id: playbackId,
    })
  }

  function stopCurrentPlayback() {
    if (activePlayback.value) {
      activePlayback.value.audio.pause()
      activePlayback.value.audio.src = ''
      activePlayback.value = null
      isPlaying.value = false
    }
  }

  return {
    activePlayback,
    isPlaying,
    handlePlaybackMessage,
    requestPlay,
    requestStop,
    stopCurrentPlayback,
  }
}
