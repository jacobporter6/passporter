package dev.passporter.notify

/**
 * Held while the app reads an agent's notification and then posts or
 * cancels it, so that news ([NoticeNotifier]) and an answer's progress
 * (`actions.ActionFeedback`, `actions.ReconcileWorker`) never overwrite
 * each other between the look and the change.
 */
internal object NotificationLock
