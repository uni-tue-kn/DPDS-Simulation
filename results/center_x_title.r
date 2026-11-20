# Sourced by the plot_*.r scripts (after library(ggplot2)): every ggsave() then centres the x-axis
# title on the whole figure, like the legend (legend.location = "plot"). ggplot2 centres the title
# on the panel, which the y-axis labels push to the right, so title and legend did not line up and
# the title was off-centre in the paper, most visibly in the narrow QQ plots.

# Returns the plot as a gtable whose x-axis title spans all columns instead of the panel's.
center_x_title <- function(plot) {
    if (inherits(plot, "patchwork")) return(plot)
    grDevices::pdf(NULL)
    on.exit(grDevices::dev.off())
    g <- ggplotGrob(plot)
    g$layout[g$layout$name == "xlab-b", c("l", "r")] <- c(1, ncol(g))
    g
}

ggsave <- function(..., plot) ggplot2::ggsave(..., plot = center_x_title(plot))
