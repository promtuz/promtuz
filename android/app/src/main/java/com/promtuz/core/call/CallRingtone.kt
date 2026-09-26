package com.promtuz.core.call

import android.content.Context
import android.media.AudioAttributes
import android.media.Ringtone
import android.media.RingtoneManager
import android.os.Build

/**
 * The incoming-call ringtone. The Calls notification channel is silent on
 * purpose (CallStyle does not ring), so the ring is played here for the length
 * of the ring and stopped the moment the call is answered, declined, or ends.
 */
object CallRingtone {
    private var ringtone: Ringtone? = null

    fun start(context: Context) {
        stop()
        runCatching {
            val uri = RingtoneManager.getActualDefaultRingtoneUri(context, RingtoneManager.TYPE_RINGTONE)
                ?: RingtoneManager.getDefaultUri(RingtoneManager.TYPE_RINGTONE)
                ?: return
            val r = RingtoneManager.getRingtone(context, uri) ?: return
            r.audioAttributes = AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                .build()
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) r.isLooping = true
            r.play()
            ringtone = r
        }
    }

    fun stop() {
        runCatching { ringtone?.stop() }
        ringtone = null
    }
}
