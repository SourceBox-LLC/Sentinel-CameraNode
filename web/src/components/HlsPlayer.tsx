// Thin wrapper around HLS.js with native-HLS fallback for Safari.
// Used by the live camera grid (live HLS) and the recording-playback
// modal (VOD HLS) — same component, different src URLs.

import { useEffect, useRef } from "react"
import Hls from "hls.js"

interface HlsPlayerProps {
  src: string
  className?: string
  autoPlay?: boolean
  muted?: boolean
  controls?: boolean
}

export default function HlsPlayer({
  src,
  className,
  autoPlay = true,
  muted = true,
  controls = false,
}: HlsPlayerProps) {
  const videoRef = useRef<HTMLVideoElement>(null)

  useEffect(() => {
    const video = videoRef.current
    if (!video) return

    // Native HLS (Safari + iOS) — feed the URL directly.
    if (video.canPlayType("application/vnd.apple.mpegurl")) {
      video.src = src
      if (autoPlay) {
        void video.play().catch(() => {
          // Autoplay can be blocked; the controls (or user interaction)
          // will resume.  Don't surface an error for this.
        })
      }
      return () => {
        video.removeAttribute("src")
        video.load()
      }
    }

    // hls.js path (Chrome / Firefox / Edge).
    if (Hls.isSupported()) {
      const hls = new Hls({
        // Live tuning — fail fast on stalls so the operator sees a
        // black tile instead of a spinning forever.  For VOD playback
        // these knobs don't fire.
        liveSyncDurationCount: 3,
        manifestLoadingTimeOut: 8_000,
        manifestLoadingMaxRetry: 1,
      })
      hls.loadSource(src)
      hls.attachMedia(video)
      hls.on(Hls.Events.MEDIA_ATTACHED, () => {
        if (autoPlay) {
          void video.play().catch(() => undefined)
        }
      })
      return () => {
        hls.destroy()
      }
    }

    // Final fallback: just set the src and hope the browser figures
    // it out.  Nothing else to wire up.
    video.src = src
    return () => {
      video.removeAttribute("src")
      video.load()
    }
  }, [src, autoPlay])

  return (
    <video
      ref={videoRef}
      className={className}
      muted={muted}
      controls={controls}
      playsInline
    />
  )
}
