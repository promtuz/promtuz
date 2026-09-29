package com.promtuz.chat.update

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import androidx.core.content.IntentCompat
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/** Where an update install reports back. Not exported: only the installer, holding our own PendingIntent, sends to it. */
class InstallStatusReceiver : BroadcastReceiver(), KoinComponent {
    private val updates: UpdateRepository by inject()

    override fun onReceive(context: Context, intent: Intent) {
        when (val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                updates.installNeedsConfirmation()
                IntentCompat.getParcelableExtra(intent, Intent.EXTRA_INTENT, Intent::class.java)
                    ?.let { context.startActivity(it.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }
            }
            // Android replaces this process; nothing is left to do.
            PackageInstaller.STATUS_SUCCESS -> {}
            else -> updates.installFailed(status, intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE))
        }
    }
}
