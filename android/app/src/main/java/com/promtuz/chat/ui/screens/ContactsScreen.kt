package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ContactsVM
import com.promtuz.chat.presentation.viewmodel.SendState
import com.promtuz.chat.presentation.viewmodel.UiContact
import com.promtuz.chat.ui.components.AppDropMenu
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.SimpleScreen
import org.koin.androidx.compose.koinViewModel

@Composable
fun ContactsScreen(viewModel: ContactsVM = koinViewModel()) {
    val contacts by viewModel.contacts.collectAsState()

    SimpleScreen({ Text("Contacts") }) { padding ->
        LazyColumn(
            Modifier
                .fillMaxSize()
                .padding(padding),
            contentPadding = PaddingValues(16.dp, 8.dp, 16.dp, 40.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp)
        ) {
            item { ContactsSummary(contacts) }
            items(contacts, key = { it.ipkHex }) { contact ->
                ContactCard(contact, onForget = { viewModel.forget(contact.ipkHex) })
            }
            if (contacts.isEmpty()) {
                item {
                    Text(
                        "No contacts yet. Scan a QR to add one.",
                        Modifier.padding(top = 24.dp),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        style = MaterialTheme.typography.bodyLarge
                    )
                }
            }
        }
    }
}

@Composable
private fun ContactsSummary(contacts: List<UiContact>) {
    val paired = contacts.count { it.paired }
    Text(
        "${contacts.size} contacts · $paired paired",
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.labelLarge
    )
}

@Composable
private fun ContactCard(contact: UiContact, onForget: () -> Unit) {
    val colors = MaterialTheme.colorScheme

    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(20.dp))
            .background(colors.surfaceContainerLow)
            .padding(14.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp)
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Avatar(contact.name, size = 40.dp)
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f)) {
                Text(
                    contact.name,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                    color = colors.onSurface
                )
                Text(
                    contact.ipkHex.take(16),
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = FontFamily.Monospace,
                    color = colors.onSurfaceVariant
                )
            }
            PairedBadge(contact.paired)
            ContactMenu(onForget)
        }

        Text(
            diagnostics(contact),
            style = MaterialTheme.typography.labelSmall,
            fontFamily = FontFamily.Monospace,
            color = colors.onSurfaceVariant
        )
    }
}

@Composable
private fun PairedBadge(paired: Boolean) {
    val accent = if (paired) Color(0xFF4CAF50) else MaterialTheme.colorScheme.onSurfaceVariant
    Text(
        if (paired) "PAIRED" else "UNPAIRED",
        Modifier
            .clip(RoundedCornerShape(6.dp))
            .background(accent.copy(alpha = 0.16f))
            .padding(horizontal = 8.dp, vertical = 3.dp),
        style = MaterialTheme.typography.labelSmall,
        fontWeight = FontWeight.Bold,
        color = accent
    )
}

@Composable
private fun ContactMenu(onForget: () -> Unit) {
    val forget by rememberUpdatedState(onForget)
    val groups = remember {
        listOf(listOf(MenuAction("Forget contact", destructive = true) { forget() }))
    }
    AppDropMenu(
        anchor = {
            DrawableIcon(
                R.drawable.i_ellipsis_vertical,
                Modifier.padding(4.dp),
                desc = "Actions",
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        },
        groups = groups,
    )
}

/** Debug line: pairing/epoch, message count + newest status, pending outbox ops. */
private fun diagnostics(c: UiContact): String {
    val parts = buildList {
        add(if (c.paired) "epoch ${c.epoch ?: "?"}" else "not paired")
        add("${c.messageCount} msgs")
        c.lastStatus?.let { add("last ${it.name.lowercase()}") }
        if (c.pendingOps > 0) add("${c.pendingOps} pending")
    }
    return parts.joinToString(" · ")
}
