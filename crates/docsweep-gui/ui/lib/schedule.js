// Puts the common forms of a cron expression into words. Anything else is simply
// called custom: the next run says what matters anyway.

import { t, weekdayName } from "./i18n.js";

const isNumber = (field) => /^\d+$/.test(field);

export function describeSchedule(expression) {
  const [minute, hour, day, month, weekday] = expression.trim().split(/\s+/);
  if (month === "*" && isNumber(minute) && isNumber(hour)) {
    const time = `${hour.padStart(2, "0")}:${minute.padStart(2, "0")}`;
    if (isNumber(day) && weekday === "*") return t("schedule.monthly", { day, time });
    if (day === "*" && weekday === "*") return t("schedule.daily", { time });
    if (day === "*" && isNumber(weekday)) {
      return t("schedule.weekly", { weekday: weekdayName(Number(weekday)), time });
    }
  }
  if (month === "*" && day === "*" && weekday === "*" && hour === "*" && isNumber(minute)) {
    return t("schedule.hourly", { minute: minute.padStart(2, "0") });
  }
  return t("schedule.custom");
}
