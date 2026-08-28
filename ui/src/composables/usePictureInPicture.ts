import { ref, onBeforeUnmount } from 'vue'

export function usePictureInPicture() {
  const isPiPActive = ref(false)
  const isSupported = ref(!!document.pictureInPictureEnabled)

  function onLeavePiP() {
    isPiPActive.value = false
  }

  document.addEventListener('leavepictureinpicture', onLeavePiP)

  /**
   * FR-25 — returns why it failed instead of only warning to the console.
   * Every refusal here looks identical to the operator ("the button does
   * nothing"), so the caller needs something to show: a disabled-by-policy
   * browser, a video with no frames yet, and a rejected request are three
   * different problems.
   */
  async function requestPiP(videoElement: HTMLVideoElement): Promise<string | null> {
    if (!document.pictureInPictureEnabled) {
      return 'Picture-in-picture is disabled in this browser'
    }
    if (videoElement.readyState === 0) {
      return 'That video has not started yet'
    }
    try {
      await videoElement.requestPictureInPicture()
      isPiPActive.value = true
      return null
    } catch (err) {
      console.warn('[PiP] requestPictureInPicture failed:', err)
      return (err as Error)?.message || 'Picture-in-picture was refused'
    }
  }

  async function exitPiP() {
    if (!document.pictureInPictureElement) return
    try {
      await document.exitPictureInPicture()
      isPiPActive.value = false
    } catch (err) {
      console.warn('[PiP] exitPictureInPicture failed:', err)
    }
  }

  function cleanup() {
    document.removeEventListener('leavepictureinpicture', onLeavePiP)
  }

  onBeforeUnmount(cleanup)

  return {
    isPiPActive,
    isSupported,
    requestPiP,
    exitPiP,
  }
}
