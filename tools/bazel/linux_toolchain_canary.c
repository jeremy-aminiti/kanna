#include <gtk/gtk.h>
#include <webkit2/webkit2.h>

int main(void) {
  GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
  WebKitWebView *webview = WEBKIT_WEB_VIEW(webkit_web_view_new());
  return window != NULL && webview != NULL ? 0 : 1;
}
