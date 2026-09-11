#include <gtk/gtk.h>
#include <webkit2/webkit2.h>

int main(void) {
  return gtk_get_major_version() == 0 || webkit_get_major_version() == 0;
}
