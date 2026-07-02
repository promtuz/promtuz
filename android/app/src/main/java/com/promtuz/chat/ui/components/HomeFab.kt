package com.promtuz.chat.ui.components

import android.content.Intent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.size
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.activities.ShareIdentity

@Composable
fun HomeFab(appViewModel: AppVM) {
    val context = LocalContext.current
    val colors = MaterialTheme.colorScheme

    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(8.dp)
    ) {
        SmallFloatingActionButton({
            context.startActivity(Intent(context, ShareIdentity::class.java))
        }) {
            DrawableIcon(
                R.drawable.i_qr_code_scanner,
                desc = "QR Code",
                tint = colors.onPrimaryContainer
            )
        }
        FloatingActionButton({
            appViewModel.navigator.push(Routes.SavedUsers)
        }) {
            DrawableIcon(
                R.drawable.i_contacts,
                Modifier.size(32.dp),
                desc = "Contacts",
                tint = colors.onPrimaryContainer,
            )
        }
    }
}
