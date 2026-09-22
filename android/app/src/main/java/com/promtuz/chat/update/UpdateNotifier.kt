package com.promtuz.chat.update

import android.Manifest
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import com.promtuz.chat.LauncherActivity
import com.promtuz.chat.R
import com.promtuz.core.push.Notifications

/** Device-local history: dismissing an update must survive process death. */
internal class UpdateNotifier(private val context: Context) {
    private val manager = context.getSystemService(NotificationManager::class.java)
    private val prefs = context.getSharedPreferences("update_notifications", Context.MODE_PRIVATE)

    fun show(manifest: UpdateManifest, channel: String, required: Boolean) {
        if (manifest.versionCode <= prefs.getInt(channel, 0)) return
        Notifications.ensureChannels(context)
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED || !manager.areNotificationsEnabled() ||
            manager.getNotificationChannel(Notifications.UPDATES_CHANNEL)?.importance == NotificationManager.IMPORTANCE_NONE) return

        val intent = Intent(context, LauncherActivity::class.java)
            .setAction("com.promtuz.chat.OPEN_APP_UPDATE")
            .putExtra(EXTRA_OPEN_UPDATE, true)
            .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        val pending = PendingIntent.getActivity(
            context, NOTIFICATION_ID, intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        manager.notify(TAG, NOTIFICATION_ID,
            NotificationCompat.Builder(context, Notifications.UPDATES_CHANNEL)
                .setSmallIcon(R.drawable.i_download)
                .setContentTitle("Promtuz ${manifest.versionName} is available")
                .setContentText(if (required) "Update required to keep chatting" else "Tap to see what's new")
                .setContentIntent(pending)
                .setAutoCancel(true)
                .setOnlyAlertOnce(true)
                .setSilent(true)
                .setCategory(NotificationCompat.CATEGORY_STATUS)
                .build(),
        )
        markSeen(manifest, channel)
        prefs.edit().putInt("visible_version", manifest.versionCode)
            .putString("visible_channel", channel).commit()
    }

    fun markSeen(manifest: UpdateManifest, channel: String) {
        if (manifest.versionCode > prefs.getInt(channel, 0)) {
            prefs.edit().putInt(channel, manifest.versionCode).commit()
        }
    }

    fun clearIfInstalled(versionCode: Long, nativeChannel: String) {
        val visible = prefs.getInt("visible_version", 0)
        if (visible < versionCode ||
            (visible.toLong() == versionCode && prefs.getString("visible_channel", null) == nativeChannel)) clear()
    }

    fun clear() {
        manager.cancel(TAG, NOTIFICATION_ID)
    }

    companion object {
        const val EXTRA_OPEN_UPDATE = "open_app_update"
        private const val TAG = "app-update"
        private const val NOTIFICATION_ID = 43
    }
}
