package com.promtuz.chat.ui.components

import android.content.Intent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.state.InviteSheet
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.activities.Chat
import kotlinx.coroutines.delay

/**
 * Material 3 confirmation sheet for a remote invite link, shown over the app's
 * home. Mirrors the in-person QR pairing flow, only source-agnostic: the same
 * bytes, the same [com.promtuz.core.CoreBridge.pairFromQr] call.
 */
@Composable
fun InviteBottomSheet(vm: AppVM) {
    val state by vm.invite.collectAsState()
    val s = state ?: return
    val context = LocalContext.current
    val sheetState = rememberModalBottomSheetState()

    ModalBottomSheet(
        onDismissRequest = vm::dismissInvite,
        sheetState = sheetState,
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 24.dp)
                .padding(bottom = 32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            when (s) {
                InviteSheet.Decoding -> CircularProgressIndicator()

                InviteSheet.Invalid -> Text(
                    "This invite link is invalid.",
                    style = MaterialTheme.typography.titleMedium,
                    textAlign = TextAlign.Center,
                )

                is InviteSheet.Added -> {
                    Text(
                        "Added ✓",
                        style = MaterialTheme.typography.titleMedium,
                        textAlign = TextAlign.Center,
                    )
                    LaunchedEffect(Unit) {
                        delay(1200)
                        vm.dismissInvite()
                    }
                }

                is InviteSheet.Confirm -> when {
                    s.expired -> Text(
                        "This invite expired — ask ${s.name} for a new link.",
                        style = MaterialTheme.typography.titleMedium,
                        textAlign = TextAlign.Center,
                    )

                    s.alreadyContact -> {
                        Text(
                            "You're already connected with ${s.name}",
                            style = MaterialTheme.typography.titleMedium,
                            textAlign = TextAlign.Center,
                        )
                        Button(
                            onClick = {
                                context.startActivity(
                                    Intent(context, Chat::class.java)
                                        .putExtra("user", s.ipk)
                                        .putExtra("name", s.name)
                                )
                                vm.dismissInvite()
                            },
                            Modifier.fillMaxWidth(),
                        ) { Text("Open chat") }
                    }

                    else -> {
                        Text(
                            s.name,
                            style = MaterialTheme.typography.headlineSmall,
                            textAlign = TextAlign.Center,
                        )
                        Text(
                            "wants to connect",
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            textAlign = TextAlign.Center,
                        )
                        Button(
                            onClick = { vm.acceptInvite(s.bytes, s.name) },
                            Modifier.fillMaxWidth(),
                        ) { Text("Add") }
                        OutlinedButton(
                            onClick = vm::dismissInvite,
                            Modifier.fillMaxWidth(),
                        ) { Text("Not now") }
                    }
                }
            }
        }
    }
}
