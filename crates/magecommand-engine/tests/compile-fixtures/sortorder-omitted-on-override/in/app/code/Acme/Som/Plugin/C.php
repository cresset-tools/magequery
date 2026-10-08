<?php

namespace Acme\Som\Plugin;

class C
{
    public function afterAct($subject, int $result): int
    {
        return $result;
    }
}
